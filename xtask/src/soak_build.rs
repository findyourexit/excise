//! What `cargo xtask soak` runs before the soak: the lookup of `HEAD`, and the release build of
//! this checkout.
//!
//! # The build
//!
//! The other commands build with `e2e::build_release_binary`, which runs `cargo build --release`
//! and then names `<target>/release/excise` as the result. Cargo promises no such path:
//! `build.target` puts the binary in `<target>/<triple>/release/`, and `build.target-dir` and
//! `CARGO_BUILD_TARGET_DIR` move the target directory, so a stale `<repo>/target/release/excise`
//! could be run on a real tree in place of what was just built. A soak never guesses. It tells
//! cargo where to build (`--target-dir`: the directory as the prompt resolved it, links followed,
//! and not as the environment spells it, because a link on the way there that was retargeted after
//! the prompt would send the build to a place that the prompt did not name), has cargo report what
//! it made (`--message-format json-render-diagnostics`: the compiler's diagnostics still go to the
//! person's terminal, and the messages go to the pipe this module reads), and takes the executable
//! of the one `compiler-artifact` message that is for the `excise` binary of this checkout, after
//! it has checked that the file is a regular file inside the target directory.
//!
//! What the build hands back is a private copy of that file, not its path. A build in the same
//! target directory (another terminal, an editor) replaces `<target>/release/excise` while a soak
//! runs, and the soak opens the binary it is given again, later, by its path: it could run, and
//! hash, another program than the one this build made. So the file is opened at the moment cargo
//! reports it, and not once the build has ended: the thread that reads cargo's output checks the
//! path and opens the file as it reads the message, while cargo is still running and holds its
//! lock on the target directory, which keeps another build from replacing the file until cargo
//! exits. (What is left is the time that thread needs to wake and open the file, in which another
//! build would have to wait for the lock, take it, and build.) The open follows no link at the end
//! of the path and waits for nothing (`open_regular_file`), and it checks on the open file that it
//! is a regular file: a link or a FIFO that something puts at the path after the check is
//! refused, and a FIFO is never waited on. After the build has ended, the open file, not its path,
//! is copied into the scratch area that the command made for the build and that nothing else
//! uses. A replacement of the path after cargo reported it is not the program that runs.
//!
//! The copy is made from a file that must not exist, and its mode is then set to exactly `0700`
//! on the open copy: the mode that a file is created with goes through the umask, and a
//! restrictive one could leave the copy unreadable or not runnable by its owner. It is made a
//! megabyte at a time, and the bound on the run is asked before each chunk, as it is of the build:
//! the person's interrupt and the deadline end a copy that is too large or too slow, and that is
//! an error that starts nothing. What a stopped copy leaves is in the scratch area, which the
//! command removes. The copy does not defend against everything: it is made after cargo has
//! exited and let go of its lock, from the file that was opened while cargo held it, and an open
//! file is the file and not its bytes. Another process that rewrites that same file in place after
//! cargo exits (truncates it, or writes over it) changes what the copy gets. A build that
//! replaces the path with another file, as cargo does, does not.
//!
//! The build hands back the copy's identity with its path ([`Built`]): its device, inode, owner,
//! mode, and change time, read from the open copy once it is whole, after its last write and its
//! last change of mode. The soak opens the path without following a link and runs nothing unless
//! the open file is that file, unchanged, before it makes its own copy and after: a file put in
//! the copy's place, or written in place, is refused. The scratch area is private to the person
//! (the command refuses one that others can change), and this is the second line behind that, as
//! the soak's own copy has one.
//!
//! The build is offline (`--offline`) and cargo's cache cleaning is off for it
//! (`CARGO_CACHE_AUTO_CLEAN_FREQUENCY=never`, set on cargo alone), so it downloads nothing into
//! cargo's home directory and cleans nothing out of it. A crate that is missing from the cache
//! fails the build, and the error says to run `cargo fetch --locked`.
//!
//! ## What the build is isolated from, and what it is not
//!
//! The command tells the person where cargo and the soak write: the target directory and cargo's
//! home directory (and the scratch directory, which it has judged to lie outside the root). For
//! that to be true of what cargo writes, the build must not write anywhere else because of the
//! person's own settings, so some of them are overridden, for cargo alone:
//!
//! * `CARGO_HOME` is cargo's home directory as the prompt resolved it, links followed, so that a
//!   link on the way to it that was retargeted after the prompt cannot send cargo's usage
//!   database, lock files, and unpacked crates to a place that the prompt did not name.
//! * `TMPDIR` is the `tmp` directory of the scratch area, so that the compiler and the linker make
//!   their temporary files there and not wherever the shell points, which can be inside the tree.
//!   The command removes the scratch area when it ends, on every way out, and reports a removal
//!   that fails.
//! * `RUSTC_WRAPPER` and `RUSTC_WORKSPACE_WRAPPER` are set to the empty string, which the Cargo
//!   book documents as overriding the configuration (`build.rustc-wrapper` and
//!   `build.rustc-workspace-wrapper`, in any `.cargo/config.toml`) and resetting cargo to no
//!   wrapper, and `CARGO_BUILD_RUSTC_WRAPPER` and `CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER` are
//!   removed. A cache such as `sccache` is therefore never run by this build, and neither it nor
//!   its server writes anything. The variables that configure such a cache (`SCCACHE_DIR` and the
//!   like) are left as they are: the program they configure does not run, and taking them away
//!   could only send another program that reads them to its default directory, which can be
//!   inside the root.
//! * `CARGO_BUILD_BUILD_DIR` is the target directory, so that the intermediate artifacts go where
//!   `--target-dir` sends the artifacts, whatever `build.build-dir` says. (`--target-dir` itself
//!   overrides `CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR`, and `build.target-dir`.)
//!
//! The build is *not* confined, and nothing here can confine it. Cargo runs what the checkout and
//! the person's own cargo configuration name, with the person's rights: the build scripts and the
//! procedural macros of every crate in the build, a linker or a runner that the configuration
//! names, `RUSTC`, `RUSTFLAGS` (which can have the linker write a file anywhere), the `[env]`
//! table (which can force `TMPDIR` too), every other `CARGO_*` variable, and the
//! `.cargo/config.toml` files above the checkout and in cargo's home directory. What such a
//! program writes is up to it. The command neutralizes what it knows how to, which is the list
//! above and the `GIT_*` variables of the lookup of `HEAD`, and it runs the build in a process
//! group that it ends with the build; it does not claim more, and the prompt says that the rest is
//! not confined.
//!
//! # Supervision
//!
//! Every program here runs in a process group that this module holds, and is looked at every 25 ms:
//! its exit, the person's interrupt, and the deadline of the run. The group is led by a process of
//! its own that does nothing, `cat`, which is started first with its standard input held open
//! here (it waits for that, and ends by itself if this process goes away). The program joins the
//! group, and so does everything the program starts without leaving it. When the program has
//! exited, and when the interrupt or the deadline ends the wait, the whole group is killed
//! (`SIGKILL`), the program is killed itself too, and only then are the program and the leader
//! reaped. So a program that a compiler or a build script left running in the group does not
//! outlive the build, whether the build succeeded or failed, and a group id is signalled only
//! while it is still held: a process id is given to another process once it has been waited for,
//! and the leader, whose id is the group's, is waited for last. (The headless runner gets the same
//! guarantee by observing the exit with `waitid` and `WNOWAIT`; this crate has no binding for
//! that, so a process holds the group instead.) The program is killed itself even when the signal
//! to its group went out: one that left the group by making a session of its own is not in it,
//! and waiting for it, if it hangs, would never end. What a program starts and puts in another
//! session or process group, such as a daemon that a build script deliberately makes, is out of
//! reach: it is not signalled, and nothing waits for it.
//!
//! No wait after a kill is unbounded. A program that SIGKILL does not end is stuck where a signal
//! does not reach it (a hung network mount in the checkout or in cargo's home): once it has been
//! killed and [`REAP_GRACE`] has passed, the supervisor reports that the program could not be
//! ended, leaves it running and unreaped, and never signals it again. The bound on the run and the
//! interrupt end the command, and not only the program.
//!
//! While a program is the supervisor's to end, the interrupt is told so (a `Supervision`, taken
//! before the program is started, and refused when the person has already asked the soak to stop),
//! and a second press does not end the command: the build runs in a process group that the signal
//! of a terminal does not reach, so a command that ended then would leave it running. The guard is
//! let go of as soon as the program and its group have been ended and the program has been
//! reaped, or given up on, and before the rest of its output is read, the binary is copied, or the
//! scratch area is removed: that is file system work, which can block for as long as a file
//! system takes to answer, and a second press must be able to end a command that it blocks.
//!
//! The output of the program is read on a thread of its own, a line at a time, so a program that
//! keeps the pipe open never holds the supervisor: once the group has been killed, the end of the
//! output is awaited for 2 seconds at most. At most 256 KiB of a line is read into memory (the
//! rest of a longer line is read and dropped, and the line is counted as cut), the supervisor is
//! handed only the first lines that it asked for (the build asks for none, and the lookup of
//! `HEAD` for two), and the thread shows each whole line to a watcher before it goes on (the
//! build opens the executable it reports that way, see above). What a program writes can
//! therefore neither fill the memory of the command nor keep it from looking at the deadline.
//! Cargo's standard error is the person's own: they see what cargo prints.
//!
//! # The lookup of `HEAD`
//!
//! `git rev-parse HEAD` is run in the checkout that is built, not in the directory the command
//! happens to run in, with every variable of git's own removed (every one whose name starts with
//! `GIT_`: `GIT_DIR` and `GIT_WORK_TREE` choose another repository, and `GIT_TRACE` and
//! `GIT_TRACE2_EVENT` name a file that git writes, which could be inside the tree), and under the
//! same supervision: the bound on the run and the person's interrupt end it too. Git's own
//! configuration files are the person's, and are read as they are.

use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, BufReader, Read, Write},
    mem,
    path::{Path, PathBuf},
    process::{Child, ChildStdout, Command, ExitStatus, Stdio},
    sync::{
        Arc, Mutex, PoisonError, Weak,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

use excise_harness::{
    safety::{FileIdentity, Scratch},
    soak::{Interrupt, Interrupted, Supervision, open_regular_file, safe_path_text},
};
use serde_json::Value;

/// How often the supervisor looks at the person's interrupt, the deadline, and the program.
const POLL: Duration = Duration::from_millis(25);

/// How long a program that has been killed is given to be gone. After this nothing waits for it
/// any longer, and nothing signals it again.
const REAP_GRACE: Duration = Duration::from_secs(5);

/// How often a program that has been killed is looked at while it is waited for.
const REAP_POLL: Duration = Duration::from_millis(5);

/// How long the supervisor waits for the end of a program's output once its group has been killed.
/// Everything the program wrote is already in the pipe and the reader hands it over at once, so
/// the end comes at once too, unless a program that left the group still holds the pipe open: then
/// this is all the wait it costs.
const DRAIN: Duration = Duration::from_secs(2);

/// The most bytes of one line of a program's output that are read into memory. The rest of a longer
/// line is read and dropped. A `compiler-artifact` message is a few kilobytes, and a build script
/// that makes cargo write a line of megabytes cannot make the command hold it.
const LINE_LIMIT: usize = 1 << 18;

/// How many of the first lines of a program's output the lookup of `HEAD` keeps: none, one, or
/// more than one is all it has to tell apart.
const KEPT_LINES: usize = 2;

/// How many bytes of the binary are copied between two looks at the interrupt and the clock.
const COPY_CHUNK: usize = 1 << 20;

/// The setting that turns cargo's automatic cleaning of its cache off. The build alone gets it.
const AUTO_CLEAN_VARIABLE: &str = "CARGO_CACHE_AUTO_CLEAN_FREQUENCY";

/// The variables that make cargo run a compiler wrapper. The build sets them to the empty string,
/// which overrides the configuration and means no wrapper.
const WRAPPER_VARIABLES: [&str; 2] = ["RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"];

/// The other spellings of the same two settings. The build does not have them.
const WRAPPER_SETTINGS: [&str; 2] = [
    "CARGO_BUILD_RUSTC_WRAPPER",
    "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
];

/// What the name of every variable of git's own starts with. Git reads `GIT_DIR` and
/// `GIT_WORK_TREE`, which choose another repository, and `GIT_TRACE` and its kin, which name a
/// file that git writes.
const GIT_PREFIX: &[u8] = b"GIT_";

/// The build, as the sentences about an interrupt and the bound call it.
const BUILD: &str = "the build";

/// The lookup of `HEAD`, as the sentences about an interrupt and the bound call it.
const LOOKUP: &str = "the lookup of HEAD";

/// The copy of the binary, as the sentences about an interrupt and the bound call it.
const COPY: &str = "the copy of the binary";

/// What the build needs.
pub(crate) struct Build<'a> {
    /// The checkout: where cargo runs, and whose `Cargo.toml` owns the binary.
    pub repo: &'a Path,
    /// The directory cargo is told to build in (`--target-dir`), as the prompt resolved it, links
    /// followed. The binary must lie inside it.
    pub target: &'a Path,
    /// Cargo's home directory as the prompt resolved it, links followed: the build is given it as
    /// `CARGO_HOME`, so that no link that changes after the prompt leads cargo's files elsewhere.
    pub cargo_home: &'a Path,
    /// The scratch area the command made for the build, in the scratch directory, outside the root:
    /// the build is given its `tmp` directory for its temporary files (`TMPDIR`), and the private
    /// copy of the binary is made in it. The command removes it when it ends.
    pub scratch: &'a Scratch,
    /// When the bound on the run passes. The build is ended at that moment.
    pub deadline: Instant,
    /// The person's interrupt. The build is ended when it is set.
    pub interrupt: &'a Interrupt,
}

/// The executable that cargo reported for the `excise` binary of this checkout, opened by the
/// thread that reads cargo's output at the moment it read the report.
#[derive(Debug)]
struct Executable {
    /// Where cargo said it is: a canonical path inside the target directory.
    path: PathBuf,
    /// The file as it was when the report was read, whatever is at `path` later.
    file: fs::File,
}

/// What the thread that reads cargo's output found in it, which the build shares with that thread:
/// the executables that cargo reported, each as the thread opened it (or why it cannot be the
/// program), in the order they were read.
#[derive(Clone, Default)]
struct Reports(Arc<Mutex<Vec<io::Result<Executable>>>>);

impl Reports {
    /// Adds what the reader made of a report.
    fn add(&self, report: io::Result<Executable>) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(report);
    }

    /// Takes everything that was added.
    fn take(&self) -> Vec<io::Result<Executable>> {
        mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// The private copy of the binary that a build made: where it is, and what it is. The identity is
/// read from the open copy once it was whole, and the soak is handed both, so that the file that it
/// opens at `path` is checked to be the one that was made here, and not whatever has come to be at
/// that path since.
#[derive(Debug)]
pub(crate) struct Built {
    /// Where the copy is, in the scratch area.
    pub path: PathBuf,
    /// What the copy was when it was whole.
    pub identity: FileIdentity,
}

/// Builds the release binary of this checkout and returns a private copy of it, in the scratch
/// area. What was copied is the executable that cargo reported for this build, which is a
/// regular file inside the target directory, and which was opened at the moment cargo reported it
/// (see the module documentation). The copy is made under the same bound as the build.
///
/// A build that was interrupted or that passed the deadline was ended, with its process group, and
/// is an error: the soak starts nothing after it, and neither does a copy that was stopped. A build
/// that ended, successfully or not, left nothing running in its process group.
pub(crate) fn build(request: &Build<'_>) -> io::Result<Built> {
    let cargo = env::var_os("CARGO")
        .filter(|cargo| !cargo.is_empty())
        .unwrap_or_else(|| OsString::from("cargo"));
    let tmpdir = request.scratch.tmp();
    let mut command = cargo_command(
        &cargo,
        request.repo,
        request.target,
        request.cargo_home,
        &tmpdir,
    );
    let bound = Bound {
        deadline: request.deadline,
        interrupt: request.interrupt,
        what: BUILD,
    };
    let manifest = request.repo.join("Cargo.toml");
    let manifest = fs::canonicalize(&manifest).unwrap_or(manifest);
    let target = request.target.to_path_buf();
    let reports = Reports::default();
    let seen = reports.clone();
    // The executable is opened by the thread that reads cargo's output, as cargo reports it, and
    // the build keeps no line of that output for itself.
    let supervised = supervise_watching(&mut command, &bound, 0, move |line| {
        if let Some(executable) = reported_binary(line, &manifest) {
            seen.add(open_checked(&executable, &target));
        }
    })?;
    if !supervised.status.success() {
        return Err(io::Error::other(format!(
            "building the release binary failed ({}).\n\
             The build is offline and downloads nothing. If cargo says that a crate is\n\
             missing from its cache, run `cargo fetch --locked` yourself and then run\n\
             the soak again.",
            supervised.status
        )));
    }
    let executable = the_executable(reports.take(), supervised.cut)?;
    // The copy is part of the run: the interrupt and the deadline that end the build end it too.
    let copying = Bound {
        what: COPY,
        ..bound
    };
    let (path, identity) = private_copy(executable, request.scratch.root(), &copying)?;
    Ok(Built { path, identity })
}

/// The build, as a command: `cargo build --release --locked --offline -p excise --target-dir
/// <target> --message-format json-render-diagnostics`, run in the checkout, with the settings that
/// keep what cargo writes to the places the command names (see the module documentation): its home
/// directory in `cargo_home`, its temporary files in `tmpdir`, its intermediate artifacts in
/// `target`, no cache cleaning, and no compiler wrapper.
fn cargo_command(
    cargo: &OsStr,
    repo: &Path,
    target: &Path,
    cargo_home: &Path,
    tmpdir: &Path,
) -> Command {
    let mut command = Command::new(cargo);
    command
        .current_dir(repo)
        .args(["build", "--release", "--locked", "--offline"])
        .args(["-p", "excise", "--target-dir"])
        .arg(target)
        .args(["--message-format", "json-render-diagnostics"])
        .env(AUTO_CLEAN_VARIABLE, "never")
        .env("CARGO_HOME", cargo_home)
        .env("TMPDIR", tmpdir)
        .env("CARGO_BUILD_BUILD_DIR", target)
        // Nothing the build runs may read the person's terminal, and cargo's own output is theirs.
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    for variable in WRAPPER_VARIABLES {
        command.env(variable, "");
    }
    for variable in WRAPPER_SETTINGS {
        command.env_remove(variable);
    }
    command
}

/// The commit that the checkout `repo` is at: what `git rev-parse HEAD` prints there, as 40
/// lowercase hexadecimal digits.
///
/// It is looked up under the supervision of the build: the interrupt and the deadline end it, with
/// its process group, and that is an error.
pub(crate) fn head_commit(
    repo: &Path,
    deadline: Instant,
    interrupt: &Interrupt,
) -> io::Result<String> {
    let bound = Bound {
        deadline,
        interrupt,
        what: LOOKUP,
    };
    head_with(&mut git_command(repo), &bound)
}

/// `git rev-parse HEAD`, run in `repo`, without any variable of git's own that this process has:
/// the commit is that of the checkout that is built, whatever directory the command runs in and
/// whatever the shell exported, and no trace of git is written to a file that the shell named.
fn git_command(repo: &Path) -> Command {
    git_command_without(repo, env::vars_os().map(|(name, _)| name))
}

/// `git rev-parse HEAD`, run in `repo`, that does not inherit those of the variables named in
/// `inherited` that are git's own (`is_a_git_variable`).
fn git_command_without(repo: &Path, inherited: impl Iterator<Item = OsString>) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .stdin(Stdio::null())
        .stderr(Stdio::inherit());
    for name in inherited.filter(|name| is_a_git_variable(name)) {
        command.env_remove(name);
    }
    command
}

/// Whether `name` is the name of a variable of git's own: one that starts with `GIT_`.
fn is_a_git_variable(name: &OsStr) -> bool {
    name.as_encoded_bytes().starts_with(GIT_PREFIX)
}

/// Runs `command`, which prints a commit, and returns the commit.
fn head_with(command: &mut Command, bound: &Bound<'_>) -> io::Result<String> {
    let Supervised { status, lines, .. } = supervise(command, bound, KEPT_LINES)?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "`git rev-parse HEAD` failed ({status})"
        )));
    }
    let [line] = lines.as_slice() else {
        return Err(io::Error::other(
            "`git rev-parse HEAD` did not print exactly one line",
        ));
    };
    let head = line.trim();
    if is_commit(head) {
        Ok(head.to_owned())
    } else {
        Err(io::Error::other(
            "`git rev-parse HEAD` did not print a commit: 40 lowercase hexadecimal digits",
        ))
    }
}

/// Whether `text` is a commit as `git rev-parse HEAD` prints one, and as the document of a soak
/// holds one: 40 lowercase hexadecimal digits.
fn is_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// What ends a program before it ends by itself: the person's interrupt and the deadline of the
/// run.
struct Bound<'a> {
    deadline: Instant,
    interrupt: &'a Interrupt,
    /// The program, as the sentences about an interrupt and the bound call it.
    what: &'a str,
}

impl Bound<'_> {
    /// The error that ends the program, when it must end now.
    fn must_stop(&self) -> Option<io::Error> {
        if self.interrupt.is_set() {
            Some(self.stopped("was interrupted"))
        } else if Instant::now() >= self.deadline {
            Some(self.stopped("passed the bound on the run"))
        } else {
            None
        }
    }

    /// The error for a program that was stopped, or not started, for `because`.
    fn stopped(&self, because: &str) -> io::Error {
        io::Error::other(format!("{} {because}, so nothing was run", self.what))
    }
}

/// A program that is being supervised, and the process group it runs in.
struct Process {
    child: Child,
    /// Whether the program was killed and was still there when the grace was over. It is then left
    /// alone: ending it again signals and waits for nothing.
    given_up: bool,
    /// The group the program runs in. It is ended before the program is reaped.
    #[cfg(unix)]
    group: Group,
    /// Says that the soak has a program to end, for as long as it is held. Let go of by
    /// [`Process::end_within`] as soon as the program and its group have been ended and the
    /// program has been waited for, or given up on: the exit is armed then, if the person has
    /// asked to stop, and a second signal can end the command without leaving the program running.
    /// What the build does after that (the rest of its output, the copy of the binary, the removal
    /// of its scratch area) is file system work that can block, and the exit is not armed while a
    /// program is held.
    supervision: Option<Supervision>,
}

impl Process {
    /// Starts `command` in a process group that this holds. Nothing is left running when it fails,
    /// and nothing is started when the person has already asked the soak to stop: the error is
    /// `bound`'s own, as if the interrupt had been seen a moment before. The interrupt is told that
    /// a program is to be ended until it has been.
    fn start(command: &mut Command, bound: &Bound<'_>) -> io::Result<Self> {
        let supervision = bound
            .interrupt
            .supervising()
            .map_err(|Interrupted| bound.stopped("was interrupted"))?;
        let program = safe_path_text(Path::new(command.get_program()));
        #[cfg(unix)]
        let group = {
            use std::os::unix::process::CommandExt as _;

            let group = Group::start()?;
            command.process_group(group.id()?);
            group
        };
        let child = command
            .spawn()
            .map_err(|error| io::Error::other(format!("cannot start {program}: {error}")))?;
        Ok(Self {
            child,
            given_up: false,
            #[cfg(unix)]
            group,
            supervision: Some(supervision),
        })
    }

    /// Ends the program with its whole process group, whatever it started and left running in it,
    /// and then reaps it, waiting [`REAP_GRACE`] at most. The group is killed first and the program
    /// is waited for after: a process id is only given to another process once it has been waited
    /// for. The program is killed itself as well, whether or not the signal to its group went out:
    /// one that left the group by making a session of its own is not in it. A program that is still
    /// there when the grace is over is stuck where a signal does not reach it: it is left running
    /// and unreaped, the error says so, and ending it again signals and waits for nothing. Ending a
    /// program that has ended does nothing more.
    fn end(&mut self) -> io::Result<ExitStatus> {
        self.end_within(REAP_GRACE, &mut |child| child.try_wait())
    }

    /// [`Process::end`] with the grace, and the question that asks whether the program is gone,
    /// given: so that a test can stand in for a program that the kill does not end.
    ///
    /// The guard that says the program is the soak's to end is let go of when this returns,
    /// whether the program was reaped or was given up on: it is no longer the soak's to end, and
    /// the exit is armed then, if the person has asked to stop.
    fn end_within(
        &mut self,
        grace: Duration,
        gone: &mut dyn FnMut(&mut Child) -> io::Result<Option<ExitStatus>>,
    ) -> io::Result<ExitStatus> {
        if self.given_up {
            return Err(not_ended(grace));
        }
        #[cfg(unix)]
        self.group.end();
        let _ = self.child.kill();
        let child = &mut self.child;
        let ended = reap_within(|| gone(child), grace);
        self.given_up = ended.is_err();
        self.supervision = None;
        ended
    }
}

/// Waits for a program that has been killed, `grace` at most, asking `try_wait` whether it is gone
/// every [`REAP_POLL`]: a wait with no bound never returns for a program that the kill cannot end.
///
/// # Errors
///
/// Returns the error that the program could not be ended, when it was still there after `grace`,
/// and any error that `try_wait` returns.
fn reap_within(
    mut try_wait: impl FnMut() -> io::Result<Option<ExitStatus>>,
    grace: Duration,
) -> io::Result<ExitStatus> {
    let until = Instant::now() + grace;
    loop {
        if let Some(status) = try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= until {
            return Err(not_ended(grace));
        }
        thread::sleep(REAP_POLL);
    }
}

/// The error for a program that the kill did not end.
fn not_ended(grace: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "the program could not be ended: it was still there {} s after it was killed, so it \
             is left running and unreaped",
            grace.as_secs_f64()
        ),
    )
}

/// `error`, which is why a program was ended, and what came of ending it: when it could not be
/// ended, the error says so as well.
fn with_how_it_ended(error: io::Error, ended: io::Result<ExitStatus>) -> io::Error {
    match ended {
        Ok(_) => error,
        Err(stuck) => io::Error::new(error.kind(), format!("{error}; and {stuck}")),
    }
}

impl Drop for Process {
    /// A return path that did not end the program ends it.
    fn drop(&mut self) {
        let _ = self.end();
    }
}

/// A process group that this process holds for as long as it lives. Its leader is a process that
/// does nothing and that nobody waits for until the group has been killed, so the id of the group,
/// which is the leader's own, cannot belong to another process in the meantime, whether or not the
/// program in the group has been waited for.
#[cfg(unix)]
struct Group {
    leader: Child,
    /// The standard input of the leader, held open: `cat` waits for it to end, and it ends if
    /// this process goes away.
    _input: std::process::ChildStdin,
    ended: bool,
}

#[cfg(unix)]
impl Group {
    fn start() -> io::Result<Self> {
        use std::os::unix::process::CommandExt as _;

        let mut command = Command::new("cat");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        let mut leader = command.spawn().map_err(|error| {
            io::Error::other(format!(
                "cannot start the process group of the program: {error}"
            ))
        })?;
        let Some(input) = leader.stdin.take() else {
            let _ = leader.kill();
            let _ = reap_within(|| leader.try_wait(), REAP_GRACE);
            return Err(io::Error::other(
                "the process group of the program has no input to wait for",
            ));
        };
        Ok(Self {
            leader,
            _input: input,
            ended: false,
        })
    }

    /// The id of the group, which is the leader's.
    fn id(&self) -> io::Result<i32> {
        i32::try_from(self.leader.id())
            .map_err(|_| io::Error::other("the process group has no usable id"))
    }

    /// Kills every process in the group, and then reaps the leader. A group that has been ended is
    /// not signalled again: its id is free to be given away.
    fn end(&mut self) {
        use excise_harness::safety::kill_process_group;

        if self.ended {
            return;
        }
        self.ended = true;
        if kill_process_group(self.leader.id()).is_err() {
            let _ = self.leader.kill();
        }
        // The leader is `cat`, which a kill ends at once. One that is not reaped within the grace is
        // left as it is, and the group is not signalled again either way.
        let _ = reap_within(|| self.leader.try_wait(), REAP_GRACE);
    }
}

#[cfg(unix)]
impl Drop for Group {
    fn drop(&mut self) {
        self.end();
    }
}

/// How a supervised program ended, and what was read from its output.
#[derive(Debug)]
struct Supervised {
    status: ExitStatus,
    /// The first lines the program wrote, as many as were asked for at most, each cut at
    /// `LINE_LIMIT` bytes.
    lines: Vec<String>,
    /// How many lines were longer than `LINE_LIMIT` bytes, and so were cut there. A cut line is
    /// never shown to the watcher: it cannot be a whole message.
    cut: u64,
}

/// [`supervise_watching`] for a program that nobody looks into while it runs.
fn supervise(command: &mut Command, bound: &Bound<'_>, keep: usize) -> io::Result<Supervised> {
    supervise_watching(command, bound, keep, |_| ())
}

/// Starts `command` with its output in a pipe and in a process group of its own, and waits for it
/// without ever blocking on the pipe. Returns how it ended, the first `keep` lines it wrote to its
/// standard output, and how many lines were cut.
///
/// `watch` is called on each whole line by the thread that reads the output, as soon as the line
/// has been read. That is how the file that a build reports is opened while the build still runs.
/// What is read is bounded: no more than `LINE_LIMIT` bytes of a line are held, and no line but the
/// first `keep` is handed to the supervisor, so a program that writes for ever fills no memory and
/// cannot keep the supervisor from looking at the deadline.
///
/// Nothing is started when the interrupt is set or the deadline has passed. Once the program has
/// exited, whether it succeeded or not, its whole process group is ended. The interrupt and the
/// deadline end it the same way, and that is an error.
fn supervise_watching(
    command: &mut Command,
    bound: &Bound<'_>,
    keep: usize,
    watch: impl FnMut(&str) + Send + 'static,
) -> io::Result<Supervised> {
    if let Some(error) = bound.must_stop() {
        return Err(error);
    }
    command.stdout(Stdio::piped());
    let mut process = Process::start(command, bound)?;
    let Some(stdout) = process.child.stdout.take() else {
        return Err(io::Error::other("the program's output is not a pipe"));
    };
    // The reader sends no more than `keep` lines, which the channel holds, so it never waits for
    // the supervisor. It stops when `present` is gone, which is when this function returns.
    let (sender, lines) = mpsc::sync_channel(keep);
    let present = Arc::new(());
    let cut = Arc::new(AtomicU64::new(0));
    let reader = Reader {
        keep,
        lines: sender,
        cut: Arc::clone(&cut),
        attending: Arc::downgrade(&present),
    };
    thread::Builder::new()
        .name("soak-build-output".to_owned())
        .spawn(move || reader.read(stdout, watch))?;
    let mut collected = Vec::new();
    let status = match wait_for_exit(&mut process, &lines, &mut collected, bound) {
        Ok(status) => status,
        // The interrupt and the deadline end the program here, and not when `process` is dropped,
        // so that a program that cannot be ended is said so.
        Err(error) => return Err(with_how_it_ended(error, process.end())),
    };
    // The program has exited, and nothing it left running may outlive it.
    process.end()?;
    if status.success() {
        wait_for_output(&lines, &mut collected, bound)?;
    }
    Ok(Supervised {
        status,
        lines: collected,
        cut: cut.load(Ordering::Relaxed),
    })
}

/// Waits for the program to exit, collecting what the reader hands over, and looks at the
/// interrupt and the deadline every [`POLL`].
fn wait_for_exit(
    process: &mut Process,
    lines: &Receiver<String>,
    collected: &mut Vec<String>,
    bound: &Bound<'_>,
) -> io::Result<ExitStatus> {
    loop {
        take(lines, collected);
        if let Some(error) = bound.must_stop() {
            return Err(error);
        }
        if let Some(status) = process.child.try_wait()? {
            return Ok(status);
        }
        thread::sleep(POLL);
    }
}

/// Waits for the end of the output of a program whose group has been killed, for as long as it
/// takes to arrive and at most [`DRAIN`].
fn wait_for_output(
    lines: &Receiver<String>,
    collected: &mut Vec<String>,
    bound: &Bound<'_>,
) -> io::Result<()> {
    let drained_by = Instant::now() + DRAIN;
    while !take(lines, collected) && Instant::now() < drained_by {
        if let Some(error) = bound.must_stop() {
            return Err(error);
        }
        thread::sleep(POLL);
    }
    Ok(())
}

/// Moves what the reader has handed over into `into`, and says whether the reader is done: the
/// output has ended and everything it held was moved. The reader hands over no more than the
/// lines that were asked for, so this never runs on.
fn take(lines: &Receiver<String>, into: &mut Vec<String>) -> bool {
    loop {
        match lines.try_recv() {
            Ok(line) => into.push(line),
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

/// What the thread that reads a program's output does with it.
struct Reader {
    /// How many of the first lines go to the supervisor.
    keep: usize,
    /// Where they go. It holds `keep` lines, which is all that is ever sent, so a send never waits.
    lines: SyncSender<String>,
    /// How many lines were cut.
    cut: Arc<AtomicU64>,
    /// Alive while the supervisor is waiting for the program: once it is gone, nobody wants what
    /// the program writes, and the reader stops at the next line.
    attending: Weak<()>,
}

impl Reader {
    /// Reads the output a line at a time, shows each whole line to `watch`, and hands the first
    /// `keep` lines over, until the output ends or the supervisor has gone. A line of more than
    /// `LINE_LIMIT` bytes is counted and not shown to `watch`: it is cut, and cannot be a whole
    /// message.
    fn read(self, stdout: ChildStdout, mut watch: impl FnMut(&str)) {
        let mut reader = BufReader::new(stdout);
        let mut line = Vec::new();
        let mut sent = 0;
        // A read that fails ends the output, as the end of it does.
        while let Ok(Some(cut)) = read_capped_line(&mut reader, LINE_LIMIT, &mut line) {
            if self.attending.strong_count() == 0 {
                return;
            }
            let text = String::from_utf8_lossy(&line);
            if cut {
                self.cut.fetch_add(1, Ordering::Relaxed);
            } else {
                watch(&text);
            }
            if sent < self.keep {
                sent += 1;
                let kept = text.into_owned();
                if self.lines.send(kept).is_err() {
                    return;
                }
            }
        }
    }
}

/// Reads the next line of `reader` into `line`, which is cleared first, as far as `limit` bytes of
/// it (its line break counts when it comes within them), and reads the rest of a longer line and
/// drops it. Returns `None` at the end of the output, and otherwise whether the line was cut.
fn read_capped_line(
    reader: &mut impl io::BufRead,
    limit: usize,
    line: &mut Vec<u8>,
) -> io::Result<Option<bool>> {
    line.clear();
    let mut any = false;
    let mut cut = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return Ok(any.then_some(cut));
        }
        any = true;
        let (used, ends) = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or((available.len(), false), |end| (end + 1, true));
        let room = limit.saturating_sub(line.len());
        cut |= used > room;
        line.extend_from_slice(&available[..used.min(room)]);
        reader.consume(used);
        if ends {
            return Ok(Some(cut));
        }
    }
}

/// The one executable that cargo reported for the `excise` binary of this checkout, among what the
/// reader made of the lines of its output: the executable that was opened, or why it cannot be the
/// program. No report, or more than one, is an error: the soak does not guess which program to
/// run. `cut` is how many lines of the output were cut, which the error for no report mentions.
fn the_executable(reported: Vec<io::Result<Executable>>, cut: u64) -> io::Result<Executable> {
    match <[io::Result<Executable>; 1]>::try_from(reported) {
        Ok([executable]) => executable,
        Err(reported) if reported.is_empty() => Err(io::Error::other(format!(
            "cargo finished the build but reported no executable for the `excise` binary of\n\
             this checkout (no `compiler-artifact` message for it), so the soak does not\n\
             know which program to run{}",
            cut_note(cut)
        ))),
        Err(reported) => Err(io::Error::other(format!(
            "cargo reported {} executables for the `excise` binary of this checkout, so the\n\
             soak does not know which program to run",
            reported.len()
        ))),
    }
}

/// The sentence that says how many lines of cargo's output were cut, to end the account of a build
/// that gave no usable report; nothing when none were.
fn cut_note(cut: u64) -> String {
    if cut == 0 {
        String::new()
    } else {
        format!(
            ".\nCargo wrote {cut} line(s) of more than {LINE_LIMIT} bytes, which are read only as \
             far as that and are never the message the soak looks for"
        )
    }
}

/// The executable that the message `line` reports, when `line` is cargo's `compiler-artifact`
/// message for the `excise` binary of the package whose manifest is `manifest`.
fn reported_binary(line: &str, manifest: &Path) -> Option<PathBuf> {
    let message: Value = serde_json::from_str(line).ok()?;
    if message.get("reason")?.as_str()? != "compiler-artifact" {
        return None;
    }
    let target = message.get("target")?;
    let is_a_binary = target
        .get("kind")?
        .as_array()?
        .iter()
        .any(|kind| kind.as_str() == Some("bin"));
    if !is_a_binary
        || target.get("name")?.as_str()? != "excise"
        || !is_this_manifest(message.get("manifest_path")?.as_str()?, manifest)
    {
        return None;
    }
    message.get("executable")?.as_str().map(PathBuf::from)
}

/// Whether `reported`, a manifest path from cargo, is `manifest`, which is canonical when it can
/// be: cargo spells the path as it found it, and the checkout can be reached through a link.
fn is_this_manifest(reported: &str, manifest: &Path) -> bool {
    let reported = Path::new(reported);
    reported == manifest || fs::canonicalize(reported).is_ok_and(|resolved| resolved == manifest)
}

/// The error for an executable that cargo reported and that is not the program: `why` completes
/// "which".
fn refusal(executable: &Path, why: &str) -> io::Error {
    io::Error::other(format!(
        "cargo reported the binary it built at\n\n    {}\n\nwhich {why}",
        safe_path_text(executable)
    ))
}

/// `executable` as the path to run, or why it cannot be: it must be a regular file and not a link,
/// and it must lie inside `target`, the directory the build was told to use. The path returned is
/// canonical.
fn checked(executable: &Path, target: &Path) -> io::Result<PathBuf> {
    let refused = |why: &str| refusal(executable, why);
    let metadata = fs::symlink_metadata(executable)
        .map_err(|error| refused(&format!("cannot be inspected: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(refused("is not a regular file"));
    }
    let resolved = fs::canonicalize(executable)
        .map_err(|error| refused(&format!("cannot be resolved: {error}")))?;
    let inside = fs::canonicalize(target).map_err(|error| {
        io::Error::other(format!(
            "cannot inspect the target directory\n\n    {}\n\n{error}",
            safe_path_text(target)
        ))
    })?;
    if !resolved.starts_with(&inside) {
        return Err(refused(&format!(
            "is outside the target directory that the build was told to use:\n\n    {}",
            safe_path_text(target)
        )));
    }
    Ok(resolved)
}

/// `executable`, which cargo reported, checked as [`checked`] checks it and then opened. The path
/// in what comes back is the canonical one, and the file is the one that was opened: whatever is
/// at the path afterwards is not.
fn open_checked(executable: &Path, target: &Path) -> io::Result<Executable> {
    open_executable(executable, checked(executable, target)?)
}

/// The file at `path`, which [`checked`] found to be a regular file inside the target directory,
/// opened: without following a link at its end, without waiting, and with a check on the open file
/// that it is a regular file (`open_regular_file`). `checked` looked at the path and this opens it,
/// and between the two something can put another entry there: a link, or a FIFO, which a plain
/// open would follow, or wait on for a writer that never comes. Here that is a refusal, at once.
/// `executable` is the path as cargo reported it, for the refusal.
fn open_executable(executable: &Path, path: PathBuf) -> io::Result<Executable> {
    let file = open_regular_file(&path)
        .map_err(|error| refusal(executable, &format!("cannot be opened as a file: {error}")))?;
    Ok(Executable { path, file })
}

/// A private copy of `executable` in `directory`, named as the file at its path is, and the path
/// and the identity of the copy. What is copied is the file that was opened when cargo reported
/// it, whatever is at its path now, and it is copied under `bound`: a stop of the bound is the
/// error of the call, as it is of the build.
fn private_copy(
    executable: Executable,
    directory: &Path,
    bound: &Bound<'_>,
) -> io::Result<(PathBuf, FileIdentity)> {
    let Executable { path, mut file } = executable;
    let name = path.file_name().unwrap_or_else(|| OsStr::new("excise"));
    let copy = directory.join(name);
    match copy_to(&mut file, &copy, bound) {
        Ok(Made::Whole(identity)) => Ok((copy, identity)),
        Ok(Made::Stopped(stop)) => Err(stop),
        Err(error) => Err(io::Error::other(format!(
            "cannot copy the binary that cargo built at\n\n    {}\n\n\
             it cannot be copied to\n\n    {}\n\n{error}",
            safe_path_text(&path),
            safe_path_text(&copy)
        ))),
    }
}

/// How a copy ended.
#[derive(Debug)]
enum Copied {
    /// Every byte was copied.
    Whole,
    /// The bound said so before a chunk, which is the error that ends the soak; the rest was not
    /// copied.
    Stopped(io::Error),
}

/// How the copy of a file into a new file ended.
#[derive(Debug)]
enum Made {
    /// Every byte was copied, and this is what the new file is: its identity, read from the open
    /// file after the last write.
    Whole(FileIdentity),
    /// The bound said so before a chunk, which is the error that ends the soak; the rest was not
    /// copied.
    Stopped(io::Error),
}

/// Copies what `original` holds into a new file at `path`, which must not exist, and which only its
/// owner can read or run: its mode is set to exactly `0700` after it is made, because the mode a
/// file is made with goes through the umask. The copy is made a chunk at a time and `bound` is
/// asked before each, the first included, so a stop that has come already makes no file. The copy
/// is closed when this returns, and its identity is read from it before that, once it is whole.
fn copy_to(original: &mut impl Read, path: &Path, bound: &Bound<'_>) -> io::Result<Made> {
    if let Some(stop) = bound.must_stop() {
        return Ok(Made::Stopped(stop));
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(0o700);
    }
    let mut copy = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        copy.set_permissions(fs::Permissions::from_mode(0o700))?;
    }
    match copy_in_chunks(original, &mut copy, COPY_CHUNK, &mut || bound.must_stop())? {
        Copied::Whole => Ok(Made::Whole(FileIdentity::of(&copy)?)),
        Copied::Stopped(stop) => Ok(Made::Stopped(stop)),
    }
}

/// Copies `from` to `to` `chunk` bytes at a time, and asks `stop` before each chunk, the first
/// included: what it returns ends the copy, as [`Copied::Stopped`], with what was copied so far
/// written.
fn copy_in_chunks(
    from: &mut impl Read,
    to: &mut impl Write,
    chunk: usize,
    stop: &mut impl FnMut() -> Option<io::Error>,
) -> io::Result<Copied> {
    let mut buffer = vec![0_u8; chunk];
    loop {
        if let Some(error) = stop() {
            return Ok(Copied::Stopped(error));
        }
        let read = match from.read(&mut buffer) {
            Ok(0) => return Ok(Copied::Whole),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        to.write_all(&buffer[..read])?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = "/work/excise/Cargo.toml";

    /// A `compiler-artifact` message, as cargo writes one: only the fields this module reads.
    fn artifact(kind: &str, name: &str, manifest: &Path, executable: Option<&Path>) -> String {
        serde_json::json!({
            "reason": "compiler-artifact",
            "package_id": "excise",
            "manifest_path": manifest,
            "target": { "kind": [kind], "crate_types": [kind], "name": name },
            "executable": executable,
            "fresh": false,
        })
        .to_string()
    }

    /// A target directory that holds `release/excise`, and the path of that file.
    fn target_with_binary() -> (tempfile::TempDir, PathBuf) {
        let target = tempfile::tempdir().expect("a directory");
        let executable = target.path().join("release").join("excise");
        fs::create_dir_all(executable.parent().expect("a parent")).expect("a directory");
        fs::write(&executable, b"a program").expect("a file");
        (target, executable)
    }

    /// What ends the build at `deadline`, or when `interrupt` is set.
    fn within(deadline: Instant, interrupt: &Interrupt) -> Bound<'_> {
        Bound {
            deadline,
            interrupt,
            what: BUILD,
        }
    }

    /// A moment far enough away that nothing in a test reaches it.
    fn far() -> Instant {
        Instant::now() + Duration::from_mins(1)
    }

    /// The names and values that `command` sets or removes, by name.
    fn settings(command: &Command) -> Vec<(String, Option<String>)> {
        let mut settings: Vec<_> = command
            .get_envs()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect();
        settings.sort();
        settings
    }

    fn setting(name: &str, value: Option<&str>) -> (String, Option<String>) {
        (name.to_owned(), value.map(str::to_owned))
    }

    #[test]
    fn cargo_is_told_to_build_offline_in_the_target_directory_and_to_report_what_it_made() {
        let command = cargo_command(
            OsStr::new("cargo"),
            Path::new("/work/excise"),
            Path::new("/work/target"),
            Path::new("/work/home/.cargo"),
            Path::new("/work/scratch"),
        );

        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments.join(" "),
            "build --release --locked --offline -p excise --target-dir /work/target \
             --message-format json-render-diagnostics"
        );
        assert_eq!(command.get_program(), OsStr::new("cargo"));
        assert_eq!(command.get_current_dir(), Some(Path::new("/work/excise")));
        assert_eq!(
            settings(&command),
            [
                setting("CARGO_BUILD_BUILD_DIR", Some("/work/target")),
                setting("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", None),
                setting("CARGO_BUILD_RUSTC_WRAPPER", None),
                setting(AUTO_CLEAN_VARIABLE, Some("never")),
                setting("CARGO_HOME", Some("/work/home/.cargo")),
                setting("RUSTC_WORKSPACE_WRAPPER", Some("")),
                setting("RUSTC_WRAPPER", Some("")),
                setting("TMPDIR", Some("/work/scratch")),
            ],
            "cargo's cache cleaning is off, its home directory, its temporary files, and its \
             intermediate artifacts are where the command says, and no compiler wrapper runs (the \
             empty string overrides the configuration, and the other spellings are removed); \
             nothing else is set or removed, the cache variables of a wrapper included"
        );
    }

    #[test]
    fn git_is_asked_for_the_commit_in_the_checkout_and_inherits_no_variable_of_its_own() {
        let inherited = [
            "PATH",
            "HOME",
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_TRACE",
            "GIT_TRACE2_EVENT",
            "GIT_CONFIG_COUNT",
            "GITHUB_TOKEN",
            "NOT_GIT_DIR",
            "git_trace",
        ];

        let command = git_command_without(
            Path::new("/work/excise"),
            inherited.into_iter().map(OsString::from),
        );

        let arguments: Vec<String> = command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert_eq!(command.get_program(), OsStr::new("git"));
        assert_eq!(arguments, ["rev-parse", "HEAD"]);
        assert_eq!(
            command.get_current_dir(),
            Some(Path::new("/work/excise")),
            "the commit is that of the checkout that is built, not of the directory the command \
             runs in"
        );
        assert_eq!(
            settings(&command),
            [
                setting("GIT_COMMON_DIR", None),
                setting("GIT_CONFIG_COUNT", None),
                setting("GIT_DIR", None),
                setting("GIT_TRACE", None),
                setting("GIT_TRACE2_EVENT", None),
                setting("GIT_WORK_TREE", None),
            ],
            "every variable whose name starts with `GIT_` is removed: one can choose another \
             repository, and another can name a file that git writes, in the tree; no other \
             variable is touched"
        );
    }

    #[test]
    fn the_lookup_of_the_commit_removes_every_variable_of_git_that_this_process_has() {
        let mut expected: Vec<String> = env::vars_os()
            .map(|(name, _)| name)
            .filter(|name| name.to_string_lossy().starts_with("GIT_"))
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        expected.sort();

        let removed: Vec<(String, Option<String>)> =
            settings(&git_command(Path::new("/work/excise")));

        assert_eq!(
            removed,
            expected
                .iter()
                .map(|name| setting(name, None))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn only_the_artifact_of_the_excise_binary_of_this_checkout_names_the_program() {
        let manifest = Path::new(MANIFEST);
        let program = Path::new("/work/target/x86_64-unknown-test/release/excise");

        assert_eq!(
            reported_binary(
                &artifact("bin", "excise", manifest, Some(program)),
                manifest,
            ),
            Some(program.to_path_buf())
        );
        for (line, why) in [
            (
                artifact("lib", "excise", manifest, None),
                "the library of the package has no executable",
            ),
            (
                artifact("bin", "excise", manifest, None),
                "a binary that cargo gave no executable for",
            ),
            (
                artifact(
                    "bin",
                    "excise-shape",
                    manifest,
                    Some(Path::new("/work/target/release/excise-shape")),
                ),
                "another binary of the package",
            ),
            (
                artifact(
                    "bin",
                    "excise",
                    Path::new("/elsewhere/Cargo.toml"),
                    Some(Path::new("/elsewhere/target/release/excise")),
                ),
                "the binary of another package",
            ),
            (
                r#"{"reason":"build-finished","success":true}"#.to_owned(),
                "another message",
            ),
            ("not a message at all".to_owned(), "output that is not JSON"),
            (String::new(), "an empty line"),
        ] {
            assert_eq!(reported_binary(&line, manifest), None, "{why}");
        }
    }

    #[test]
    fn exactly_one_reported_executable_is_the_program() {
        let (target, executable) = target_with_binary();
        let reported = || open_checked(&executable, target.path());
        let not_there = target.path().join("release").join("nothing");

        let program = the_executable(vec![reported()], 0).expect("one binary was reported");
        let none = the_executable(Vec::new(), 0).expect_err("no binary");
        let two = the_executable(vec![reported(), reported()], 0).expect_err("two binaries");
        let unusable = the_executable(vec![open_checked(&not_there, target.path())], 0)
            .expect_err("a binary that is not there");
        let after_cut_lines =
            the_executable(Vec::new(), 2).expect_err("no binary, and lines that were cut");

        assert_eq!(
            program.path,
            fs::canonicalize(&executable).expect("the file exists")
        );
        assert!(
            none.to_string().contains("reported no executable"),
            "{none}"
        );
        assert!(
            !none.to_string().contains("line(s)"),
            "no line was cut, so none is mentioned: {none}"
        );
        assert!(two.to_string().contains("reported 2 executables"), "{two}");
        assert!(
            unusable.to_string().contains("cannot be inspected"),
            "{unusable}"
        );
        assert!(
            after_cut_lines
                .to_string()
                .contains("Cargo wrote 2 line(s) of more than 262144 bytes"),
            "{after_cut_lines}"
        );
    }

    #[test]
    fn the_reported_executable_must_be_a_regular_file_inside_the_target_directory() {
        let (target, executable) = target_with_binary();
        let elsewhere = tempfile::tempdir().expect("another directory");
        let outside = elsewhere.path().join("excise");
        fs::write(&outside, b"a program").expect("a file");

        let missing = checked(
            &target.path().join("release").join("nothing"),
            target.path(),
        )
        .expect_err("no such file");
        let directory =
            checked(&target.path().join("release"), target.path()).expect_err("a directory");
        let beyond = checked(&outside, target.path()).expect_err("outside the target directory");

        assert_eq!(
            checked(&executable, target.path()).expect("inside the target directory"),
            fs::canonicalize(&executable).expect("the file exists")
        );
        assert!(
            missing.to_string().contains("cannot be inspected"),
            "{missing}"
        );
        assert!(
            directory.to_string().contains("is not a regular file"),
            "{directory}"
        );
        assert!(
            beyond
                .to_string()
                .contains("is outside the target directory that the build was told to use"),
            "{beyond}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_not_the_program_even_when_it_leads_to_one_inside_the_target_directory() {
        let (target, executable) = target_with_binary();
        let link = target.path().join("release").join("linked");
        std::os::unix::fs::symlink(&executable, &link).expect("a link");

        let refused = checked(&link, target.path()).expect_err("a link");

        assert!(
            refused.to_string().contains("is not a regular file"),
            "{refused}"
        );
    }

    /// What ends the copy of the binary at `deadline`, or when `interrupt` is set.
    fn copying(deadline: Instant, interrupt: &Interrupt) -> Bound<'_> {
        Bound {
            what: COPY,
            ..within(deadline, interrupt)
        }
    }

    #[cfg(unix)]
    #[test]
    fn the_copy_is_of_the_file_that_was_opened_and_not_of_whatever_is_at_its_path_later() {
        let (target, executable) = target_with_binary();
        let mut original = fs::File::open(&executable).expect("the file opens");
        // Another build replaces the file at the path, as cargo does: a new file is moved over it.
        let replacement = target.path().join("release").join("excise-next");
        fs::write(&replacement, b"another program").expect("a file");
        fs::rename(&replacement, &executable).expect("the path is taken by another file");
        let directory = tempfile::tempdir().expect("a directory");
        let copy = directory.path().join("excise");

        let copied =
            copy_to(&mut original, &copy, &copying(far(), &Interrupt::new())).expect("copied");

        assert!(matches!(copied, Made::Whole(_)));
        assert_eq!(
            fs::read(&copy).expect("the copy"),
            b"a program",
            "what was opened"
        );
        assert_eq!(
            fs::read(&executable).expect("the file at the path"),
            b"another program",
            "the replacement is where it was put, and is not what was copied"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_copy_is_new_and_its_owner_can_run_it_and_nobody_else_can_use_it() {
        use std::os::unix::fs::PermissionsExt as _;

        let (_target, executable) = target_with_binary();
        let directory = tempfile::tempdir().expect("a directory");
        let copy = directory.path().join("excise");
        let interrupt = Interrupt::new();
        let bound = copying(far(), &interrupt);

        let mut original = fs::File::open(&executable).expect("the file opens");
        copy_to(&mut original, &copy, &bound).expect("copied");
        let mode = fs::metadata(&copy).expect("the copy").permissions().mode();
        let mut again = fs::File::open(&executable).expect("the file opens");
        let second = copy_to(&mut again, &copy, &bound).expect_err("the path is taken");

        assert_eq!(
            mode & 0o777,
            0o700,
            "its owner can read, write, and run it, and nobody else can use it: {mode:o}"
        );
        assert_eq!(
            second.kind(),
            io::ErrorKind::AlreadyExists,
            "a file that is there is never written over"
        );
        assert_eq!(fs::read(&copy).expect("the copy"), b"a program");
    }

    /// The variable that tells the run of the next test, which a shell starts with a umask of its
    /// own, which directory to work in.
    #[cfg(unix)]
    const UMASK_DIRECTORY: &str = "XTASK_SOAK_BUILD_UMASK_TEST_DIRECTORY";

    #[cfg(unix)]
    #[test]
    fn the_copy_is_readable_and_runnable_by_its_owner_whatever_the_umask_of_the_process() {
        use std::os::unix::fs::PermissionsExt as _;

        if let Some(directory) = env::var_os(UMASK_DIRECTORY) {
            // The run in the shell. Its umask takes every bit from a new file but the owner's
            // write, so a file that is only made with the mode 0700 can be neither read nor run.
            // The directory was made by the process that started the shell, which has an ordinary
            // umask, because a directory made here could not be entered.
            let directory = PathBuf::from(directory);
            let mut original =
                fs::File::open(directory.join("original")).expect("the original opens");
            let copy = directory.join("copy");

            let copied =
                copy_to(&mut original, &copy, &copying(far(), &Interrupt::new())).expect("copied");

            assert!(matches!(copied, Made::Whole(_)));
            let mode = fs::metadata(&copy).expect("the copy").permissions().mode();
            assert_eq!(mode & 0o777, 0o700, "{mode:o}");
            let status = Command::new(&copy).status().expect("the copy runs");
            assert_eq!(
                status.code(),
                Some(7),
                "the copy is the program, and it ran"
            );
            fs::write(directory.join("ran"), b"yes").expect("a marker");
            return;
        }

        // The test program cannot change its own umask without `unsafe` code, and a umask is the
        // process's: it would change what the tests that run beside this one make. So this test
        // runs itself again, and only itself, in a shell that has set a restrictive umask.
        let directory = tempfile::tempdir().expect("a directory");
        let original = directory.path().join("original");
        fs::write(&original, "#!/bin/sh\nexit 7\n").expect("a program");
        fs::set_permissions(&original, fs::Permissions::from_mode(0o755)).expect("executable");
        let this_test =
            "the_copy_is_readable_and_runnable_by_its_owner_whatever_the_umask_of_the_process";

        let output = Command::new("/bin/sh")
            .args(["-c", "umask 0577 && exec \"$0\" --test-threads=1 \"$1\""])
            .arg(env::current_exe().expect("the test program"))
            .arg(this_test)
            .env(UMASK_DIRECTORY, directory.path())
            .output()
            .expect("the shell runs");

        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            directory.path().join("ran").exists(),
            "the check ran in the shell that set the umask"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_private_copy_is_named_as_the_binary_and_is_in_the_directory_it_is_given() {
        let (target, executable) = target_with_binary();
        let directory = tempfile::tempdir().expect("a directory");
        let interrupt = Interrupt::new();
        let bound = copying(far(), &interrupt);

        let opened = open_checked(&executable, target.path()).expect("opened");
        let (copy, identity) = private_copy(opened, directory.path(), &bound).expect("copied");
        let again = open_checked(&executable, target.path()).expect("opened again");
        let taken = private_copy(again, directory.path(), &bound).expect_err("the copy is there");

        assert_eq!(copy, directory.path().join("excise"));
        assert_eq!(
            identity.check(&copy),
            Ok(()),
            "the identity is that of the file that was made, whole"
        );
        assert_eq!(fs::read(&copy).expect("the copy"), b"a program");
        assert!(
            taken.to_string().contains("it cannot be copied to"),
            "{taken}"
        );
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("the directory")
                .count(),
            1,
            "a copy that is there is never written over, and no other file was made"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_that_is_stopped_before_it_starts_makes_no_file_and_says_what_stopped_it() {
        let (target, executable) = target_with_binary();
        let directory = tempfile::tempdir().expect("a directory");
        let flagged = Interrupt::new();
        flagged.trigger();

        let by_interrupt = private_copy(
            open_checked(&executable, target.path()).expect("opened"),
            directory.path(),
            &copying(far(), &flagged),
        )
        .expect_err("interrupted");
        let by_bound = private_copy(
            open_checked(&executable, target.path()).expect("opened"),
            directory.path(),
            &copying(Instant::now(), &Interrupt::new()),
        )
        .expect_err("out of time");

        assert_eq!(
            by_interrupt.to_string(),
            "the copy of the binary was interrupted, so nothing was run"
        );
        assert_eq!(
            by_bound.to_string(),
            "the copy of the binary passed the bound on the run, so nothing was run"
        );
        assert_eq!(
            fs::read_dir(directory.path())
                .expect("the directory")
                .count(),
            0,
            "no file was made"
        );
    }

    #[test]
    fn a_copy_asks_before_every_chunk_and_a_stop_ends_it_with_what_was_copied_so_far() {
        let source = [7_u8; 10];
        let mut written = Vec::new();
        let mut asked = 0;

        let copied = copy_in_chunks(&mut source.as_slice(), &mut written, 4, &mut || {
            asked += 1;
            (asked == 3).then(|| io::Error::other("stop"))
        })
        .expect("no read or write failed");

        assert!(matches!(copied, Copied::Stopped(_)));
        assert_eq!(asked, 3, "once before each of two chunks, and once more");
        assert_eq!(
            written.len(),
            8,
            "the chunks that were asked for were copied"
        );
    }

    #[test]
    fn a_copy_that_nothing_stops_is_whole_whatever_the_size_of_the_chunks() {
        let source: Vec<u8> = (0..=u8::MAX).cycle().take(1000).collect();

        for chunk in [1, 7, 64, 999, 1000, 1001, COPY_CHUNK] {
            let mut written = Vec::new();

            let copied = copy_in_chunks(&mut source.as_slice(), &mut written, chunk, &mut || None)
                .expect("no read or write failed");

            assert!(matches!(copied, Copied::Whole), "{chunk}");
            assert_eq!(written, source, "{chunk}");
        }
    }

    /// A file that is appended to as fast as it is copied, so that it does not seem to end. It
    /// ends after `chunks` chunks all the same, so that a copy that nothing stops comes out whole
    /// and fails the test, and does not run on. The first chunk that is read from it brings the
    /// person's interrupt, if there is one.
    struct Appended<'a> {
        served: usize,
        chunks: usize,
        interrupt: Option<&'a Interrupt>,
        pause: Duration,
    }

    impl Read for Appended<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.served == self.chunks {
                return Ok(0);
            }
            thread::sleep(self.pause);
            buffer.fill(1);
            self.served += 1;
            if let (1, Some(interrupt)) = (self.served, self.interrupt) {
                interrupt.trigger();
            }
            Ok(buffer.len())
        }
    }

    #[test]
    fn the_interrupt_ends_a_copy_between_two_chunks_even_of_a_file_that_does_not_seem_to_end() {
        let interrupt = Interrupt::new();
        let mut source = Appended {
            served: 0,
            chunks: 1000,
            interrupt: Some(&interrupt),
            pause: Duration::ZERO,
        };
        let mut written = Vec::new();
        let bound = copying(far(), &interrupt);

        let copied = copy_in_chunks(&mut source, &mut written, 16, &mut || bound.must_stop())
            .expect("no read or write failed");

        let Copied::Stopped(stop) = copied else {
            panic!("the copy of a file that is appended to was not stopped");
        };
        assert_eq!(
            stop.to_string(),
            "the copy of the binary was interrupted, so nothing was run"
        );
        assert_eq!(
            written.len(),
            16,
            "the chunk that was being read was copied, and the next was not started"
        );
    }

    #[test]
    fn the_deadline_ends_a_copy_of_a_file_that_is_slow_and_does_not_seem_to_end() {
        let never = Interrupt::new();
        let mut source = Appended {
            served: 0,
            chunks: 100,
            interrupt: None,
            pause: Duration::from_millis(20),
        };
        let mut written = Vec::new();
        let started = Instant::now();
        let bound = copying(started + Duration::from_millis(200), &never);

        let copied = copy_in_chunks(&mut source, &mut written, 16, &mut || bound.must_stop())
            .expect("no read or write failed");

        let Copied::Stopped(stop) = copied else {
            panic!("the copy of a file that is appended to was not stopped");
        };
        assert_eq!(
            stop.to_string(),
            "the copy of the binary passed the bound on the run, so nothing was run"
        );
        assert!(
            written.len() < 100 * 16,
            "the copy was cut: {} bytes",
            written.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_copy_of_a_file_that_is_appended_to_is_stopped_between_chunks_by_the_interrupt() {
        let directory = tempfile::tempdir().expect("a directory");
        let copy = directory.path().join("excise");
        let interrupt = Interrupt::new();
        // It ends after four chunks, so a copy that nothing stops makes a file of four megabytes
        // and fails the test, and not a file of a size that nothing bounds.
        let mut source = Appended {
            served: 0,
            chunks: 4,
            interrupt: Some(&interrupt),
            pause: Duration::ZERO,
        };

        let copied = copy_to(&mut source, &copy, &copying(far(), &interrupt))
            .expect("no read or write failed");

        let Made::Stopped(stop) = copied else {
            panic!("the copy of a file that is appended to was not stopped");
        };
        assert_eq!(
            stop.to_string(),
            "the copy of the binary was interrupted, so nothing was run"
        );
        assert_eq!(
            fs::metadata(&copy).expect("the part that was copied").len(),
            u64::try_from(COPY_CHUNK).expect("a size"),
            "the chunk that was being read was copied, and the next was not started"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_or_a_link_put_where_the_checked_file_was_is_refused_and_never_waited_on() {
        let directory = tempfile::tempdir().expect("a directory");
        let fifo = directory.path().join("fifo");
        let made = Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .expect("mkfifo runs");
        assert!(made.success(), "mkfifo made the FIFO");
        let regular = directory.path().join("regular");
        fs::write(&regular, b"a program").expect("a file");
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&regular, &link).expect("a link");

        for (what, planted, said) in [
            ("a FIFO", &fifo, "not a regular file"),
            ("a link", &link, "a symbolic link, which is not followed"),
        ] {
            let (sender, received) = mpsc::channel();
            let path = planted.clone();
            let opener = thread::spawn(move || {
                let result = open_executable(&path, path.clone()).map(|executable| executable.path);
                let _ = sender.send(result);
            });
            let outcome = received.recv_timeout(Duration::from_secs(10));
            if outcome.is_err() {
                // A reader that waits for a writer is let go by one, so that no thread is left.
                let _ = fs::OpenOptions::new().write(true).open(&fifo);
            }
            opener.join().expect("the thread ends");

            let refused = outcome
                .unwrap_or_else(|_| panic!("{what}: the open waited"))
                .expect_err(what);
            assert!(refused.to_string().contains(said), "{what}: {refused}");
            assert!(
                refused
                    .to_string()
                    .contains("cargo reported the binary it built at"),
                "{what}: {refused}"
            );
        }
    }

    #[test]
    fn nothing_is_opened_unless_it_is_a_regular_file_inside_the_target_directory() {
        let (target, executable) = target_with_binary();
        let elsewhere = tempfile::tempdir().expect("another directory");
        let outside = elsewhere.path().join("excise");
        fs::write(&outside, b"a program").expect("a file");

        let opened = open_checked(&executable, target.path()).expect("inside the target directory");
        let missing = open_checked(
            &target.path().join("release").join("nothing"),
            target.path(),
        )
        .expect_err("no such file");
        let directory =
            open_checked(&target.path().join("release"), target.path()).expect_err("a directory");
        let beyond =
            open_checked(&outside, target.path()).expect_err("outside the target directory");

        assert_eq!(
            opened.path,
            fs::canonicalize(&executable).expect("the file exists")
        );
        assert_eq!(
            io::read_to_string(opened.file).expect("the open file reads"),
            "a program"
        );
        assert!(
            missing.to_string().contains("cannot be inspected"),
            "{missing}"
        );
        assert!(
            directory.to_string().contains("is not a regular file"),
            "{directory}"
        );
        assert!(
            beyond
                .to_string()
                .contains("is outside the target directory that the build was told to use"),
            "{beyond}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_file_a_build_reports_is_opened_as_the_report_is_read_while_the_build_still_runs() {
        let (target, executable) = target_with_binary();
        let marker = target.path().join("opened");
        // The build reports the file, and then waits until the reader says it has opened it (the
        // marker): only then does it replace the file at the path, as another build does once
        // cargo's lock is free, and end. A reader that opened the file after the build had ended
        // would get the replacement, and would never make the marker.
        let script = format!(
            "echo '{path}'\n\
             waited=0\n\
             while [ ! -e '{marker}' ] && [ \"$waited\" -lt 200 ]; do\n\
             sleep 0.05\n\
             waited=$((waited + 1))\n\
             done\n\
             echo 'another program' > '{path}.next' && mv '{path}.next' '{path}'\n",
            path = executable.display(),
            marker = marker.display(),
        );
        let reports = Reports::default();
        let seen = reports.clone();
        let looked_in = target.path().to_path_buf();
        let signal = marker.clone();
        let watch = move |line: &str| {
            seen.add(open_checked(Path::new(line.trim()), &looked_in));
            // The file is open: the build may replace it now.
            let _ = fs::write(&signal, b"opened");
        };

        let supervised = supervise_watching(
            &mut shell(&script),
            &within(far(), &Interrupt::new()),
            0,
            watch,
        )
        .expect("the build ends");
        let reported =
            the_executable(reports.take(), supervised.cut).expect("one file was reported");
        let directory = tempfile::tempdir().expect("a directory");
        let (copy, _) = private_copy(
            reported,
            directory.path(),
            &copying(far(), &Interrupt::new()),
        )
        .expect("copied");

        assert!(supervised.status.success());
        assert_eq!(
            fs::read(&copy).expect("the copy"),
            b"a program",
            "the file as it was when it was reported"
        );
        assert_eq!(
            fs::read(&executable).expect("the file at the path"),
            b"another program\n",
            "the build replaced the file after the reader had opened it"
        );
    }

    #[test]
    fn a_build_does_not_start_when_the_interrupt_or_the_bound_has_already_come() {
        let parent = tempfile::tempdir().expect("a directory");
        let scratch = Scratch::create(parent.path()).expect("a scratch area");
        let target = Path::new("/work/target");
        let cargo_home = Path::new("/work/home/.cargo");
        let flagged = Interrupt::new();
        flagged.trigger();

        let by_interrupt = build(&Build {
            repo: Path::new("/work/excise"),
            target,
            cargo_home,
            scratch: &scratch,
            deadline: far(),
            interrupt: &flagged,
        })
        .expect_err("interrupted");
        let by_bound = build(&Build {
            repo: Path::new("/work/excise"),
            target,
            cargo_home,
            scratch: &scratch,
            deadline: Instant::now(),
            interrupt: &Interrupt::new(),
        })
        .expect_err("out of time");
        let lookup =
            head_commit(Path::new("/work/excise"), far(), &flagged).expect_err("interrupted");

        assert!(
            by_interrupt
                .to_string()
                .contains("the build was interrupted, so nothing was run"),
            "{by_interrupt}"
        );
        assert!(
            by_bound
                .to_string()
                .contains("the build passed the bound on the run, so nothing was run"),
            "{by_bound}"
        );
        assert!(
            lookup
                .to_string()
                .contains("the lookup of HEAD was interrupted, so nothing was run"),
            "{lookup}"
        );
    }

    #[test]
    fn a_program_that_cannot_start_is_named() {
        let error = supervise(
            &mut Command::new("/nonexistent/cargo"),
            &within(far(), &Interrupt::new()),
            0,
        )
        .expect_err("no such program");

        assert!(
            error
                .to_string()
                .contains("cannot start /nonexistent/cargo"),
            "{error}"
        );
    }

    #[cfg(unix)]
    fn shell(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    /// Whether the process is there and is not a zombie.
    #[cfg(unix)]
    fn alive(pid: u32) -> bool {
        let output = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .stderr(Stdio::null())
            .output()
            .expect("ps runs");
        let state = String::from_utf8_lossy(&output.stdout);
        output.status.success() && !state.trim().is_empty() && !state.trim().starts_with('Z')
    }

    /// Waits until `done`: what the supervisor ended with a signal is gone a moment later.
    #[cfg(unix)]
    fn wait_until(mut done: impl FnMut() -> bool, what: &str) {
        let give_up = Instant::now() + Duration::from_secs(10);
        while !done() {
            assert!(Instant::now() < give_up, "{what}");
            thread::sleep(Duration::from_millis(25));
        }
    }

    /// The process id that the file at `path` holds, once the program that writes it has.
    #[cfg(unix)]
    fn recorded(path: &Path) -> u32 {
        let give_up = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(pid) = fs::read_to_string(path)
                .ok()
                .and_then(|text| text.trim().parse().ok())
            {
                return pid;
            }
            assert!(Instant::now() < give_up, "no process id was recorded");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_build_that_ends_gives_its_status_and_the_lines_it_wrote() {
        let interrupt = Interrupt::new();

        let done = supervise(
            &mut shell("echo one; echo two"),
            &within(far(), &interrupt),
            KEPT_LINES,
        )
        .expect("it runs");
        let failed = supervise(
            &mut shell("echo no; exit 3"),
            &within(far(), &interrupt),
            KEPT_LINES,
        )
        .expect("a failing build is not an error of the supervisor");

        assert!(done.status.success());
        assert_eq!(done.lines, ["one\n", "two\n"]);
        assert_eq!(done.cut, 0);
        assert_eq!(failed.status.code(), Some(3));
    }

    /// Every line of `reader` as `read_capped_line` reads it with `limit`, and whether it was cut.
    fn capped_lines(reader: &mut impl io::BufRead, limit: usize) -> Vec<(String, bool)> {
        let mut lines = Vec::new();
        let mut line = Vec::new();
        while let Some(cut) = read_capped_line(reader, limit, &mut line).expect("a read") {
            lines.push((String::from_utf8_lossy(&line).into_owned(), cut));
        }
        lines
    }

    #[test]
    fn a_line_is_read_up_to_the_limit_and_the_rest_of_a_longer_one_is_dropped_whatever_the_buffer()
    {
        let bytes: &[u8] = b"short\nabcdefghij\nxy\nabcdefg\nabcdefgh\nlast";
        let expected = [
            ("short\n", false),
            ("abcdefgh", true),
            ("xy\n", false),
            ("abcdefg\n", false),
            ("abcdefgh", true),
            ("last", false),
        ]
        .map(|(line, cut)| (line.to_owned(), cut));

        assert_eq!(capped_lines(&mut &bytes[..], 8), expected);
        for capacity in [1, 2, 3, 5, 8, 64] {
            assert_eq!(
                capped_lines(&mut BufReader::with_capacity(capacity, bytes), 8),
                expected,
                "a buffer of {capacity} bytes"
            );
        }
        assert_eq!(capped_lines(&mut &b""[..], 8), []);
    }

    #[cfg(unix)]
    #[test]
    fn what_a_program_writes_is_read_to_its_end_and_the_supervisor_keeps_only_the_first_lines() {
        let seen = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&seen);

        let supervised = supervise_watching(
            &mut shell("seq 1 100000"),
            &within(far(), &Interrupt::new()),
            KEPT_LINES,
            move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            },
        )
        .expect("it runs");

        assert!(supervised.status.success());
        assert_eq!(
            supervised.lines,
            ["1\n", "2\n"],
            "no more lines are kept than were asked for"
        );
        assert_eq!(supervised.cut, 0);
        assert_eq!(
            seen.load(Ordering::Relaxed),
            100_000,
            "the watcher saw every line"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_line_of_megabytes_is_cut_and_counted_and_the_lines_after_it_are_read_whole() {
        let watched = Arc::new(Mutex::new(Vec::<usize>::new()));
        let record = Arc::clone(&watched);

        let supervised = supervise_watching(
            &mut shell("head -c 3145728 /dev/zero | tr '\\0' x; echo; echo after"),
            &within(far(), &Interrupt::new()),
            KEPT_LINES,
            move |line| record.lock().expect("the lock").push(line.len()),
        )
        .expect("it runs");

        assert_eq!(supervised.cut, 1, "one line was cut");
        assert_eq!(supervised.lines.len(), 2);
        assert_eq!(
            supervised.lines[0].len(),
            LINE_LIMIT,
            "no more of the long line is held than the limit"
        );
        assert_eq!(supervised.lines[1], "after\n");
        assert_eq!(
            *watched.lock().expect("the lock"),
            [6],
            "the watcher saw the line that follows, whole, and nothing of the cut one"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_never_stops_writing_cannot_hold_the_supervisor_past_the_deadline() {
        let started = Instant::now();

        let error = supervise(
            // Writes as fast as it can for thirty seconds, which a supervisor that looks at the
            // deadline never lets it.
            &mut shell("perl -e '$end = time + 30; print \"x\\n\" while time < $end'"),
            &within(started + Duration::from_millis(500), &Interrupt::new()),
            KEPT_LINES,
        )
        .expect_err("the bound passes");

        assert!(
            error
                .to_string()
                .contains("the build passed the bound on the run, so nothing was run"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the supervisor waited for the program"
        );
    }

    /// Makes a session of its own, which takes the program out of the process group that the
    /// supervisor holds, records its process id, and then waits.
    #[cfg(unix)]
    const LEAVES_THE_GROUP: &str = "use POSIX qw(setsid); setsid() or die; \
         open(my $f, '>', $ARGV[0]) or die; print $f $$; close($f); sleep 600;";

    /// Makes a session of its own, records its process id, and then writes a line every 50
    /// milliseconds for ever, on the standard output that it inherited.
    #[cfg(unix)]
    const WRITES_FOR_EVER: &str = "use POSIX qw(setsid); setsid() or die; \
         open(my $f, \">\", $ARGV[0]) or die; print $f $$; close($f); $| = 1; \
         while (1) { print \"tick\\n\"; select(undef, undef, undef, 0.05); }";

    #[cfg(unix)]
    #[test]
    fn a_program_that_left_the_process_group_is_still_killed_and_the_supervisor_still_returns() {
        let perl = Command::new("perl")
            .arg("-e1")
            .status()
            .expect("this test needs perl, to make a program that leaves its process group");
        assert!(perl.success());
        let directory = tempfile::tempdir().expect("a directory");
        let pid_file = directory.path().join("pid");
        let mut command = Command::new("perl");
        command.args(["-e", LEAVES_THE_GROUP]).arg(&pid_file);
        let (sender, returned) = mpsc::channel();
        let supervisor = thread::spawn(move || {
            let interrupt = Interrupt::new();
            let deadline = Instant::now() + Duration::from_secs(1);
            let ended = supervise(&mut command, &within(deadline, &interrupt), 0);
            let _ = sender.send(ended);
        });
        let pid = recorded(&pid_file);

        let outcome = returned.recv_timeout(Duration::from_secs(20));

        if outcome.is_err() {
            // The supervisor waits for a program that nothing ends. End it, so that the test
            // leaves neither the program nor the thread.
            let _ = excise_harness::safety::kill_process(pid);
        }
        supervisor.join().expect("the thread ends");
        let error = outcome
            .expect("the supervisor did not return: it waited for a program that left its group")
            .expect_err("the deadline passes");
        assert!(
            error
                .to_string()
                .contains("the build passed the bound on the run, so nothing was run"),
            "{error}"
        );
        wait_until(
            || !alive(pid),
            "the program that left the process group is still running",
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_thread_that_reads_the_output_stops_reading_once_the_supervisor_has_gone() {
        let directory = tempfile::tempdir().expect("a directory");
        let pid_file = directory.path().join("pid");
        // A program that failed after it started a daemon, which made a session of its own, as a
        // build script can: the daemon keeps the pipe open and writes for ever.
        let script = format!(
            "perl -e '{WRITES_FOR_EVER}' '{pid}' & \
             while [ ! -s '{pid}' ]; do sleep 0.02; done; exit 3",
            pid = pid_file.display()
        );
        let seen = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&seen);

        let supervised = supervise_watching(
            &mut shell(&script),
            &within(far(), &Interrupt::new()),
            0,
            move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            },
        )
        .expect("a failing program is not an error of the supervisor");
        let daemon = recorded(&pid_file);
        let after_return = seen.load(Ordering::Relaxed);
        thread::sleep(Duration::from_millis(400));
        let later = seen.load(Ordering::Relaxed);
        let _ = excise_harness::safety::kill_process(daemon);

        assert_eq!(supervised.status.code(), Some(3));
        assert!(
            later <= after_return + 1,
            "the reader went on after the supervisor had gone: {after_return} lines, then {later}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_deadline_ends_a_build_that_does_not_end() {
        let started = Instant::now();

        let error = supervise(
            &mut shell("sleep 30 & wait"),
            &within(started + Duration::from_millis(300), &Interrupt::new()),
            0,
        )
        .expect_err("the bound passes");

        assert!(
            error
                .to_string()
                .contains("the build passed the bound on the run, so nothing was run"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the supervisor waited for the build"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupt_ends_a_build_that_does_not_end() {
        let interrupt = Interrupt::new();
        let trigger = interrupt.clone();
        let started = Instant::now();
        let ringer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            trigger.trigger();
        });

        let error = supervise(&mut shell("sleep 30 & wait"), &within(far(), &interrupt), 0)
            .expect_err("interrupted");

        ringer.join().expect("the thread ends");
        assert!(
            error
                .to_string()
                .contains("the build was interrupted, so nothing was run"),
            "{error}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the supervisor waited for the build"
        );
    }

    #[cfg(unix)]
    #[test]
    fn what_a_build_left_running_is_ended_with_the_build_and_the_supervisor_is_not_held_by_it() {
        let started = Instant::now();

        // The shell prints the id of its child and ends successfully; the child keeps the pipe
        // open and runs on, until the supervisor ends the group it is in.
        let Supervised { status, lines, .. } = supervise(
            &mut shell("sleep 30 & echo $!"),
            &within(far(), &Interrupt::new()),
            KEPT_LINES,
        )
        .expect("the build ends");

        let [child] = lines.as_slice() else {
            panic!("one line, the id of the child: {lines:?}");
        };
        let child: u32 = child.trim().parse().expect("a process id");
        assert!(status.success());
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the supervisor waited for the end of the output"
        );
        wait_until(
            || !alive(child),
            "the program that the build left running is still running",
        );
    }

    #[cfg(unix)]
    #[test]
    fn what_a_failed_build_left_running_is_ended_too() {
        let directory = tempfile::tempdir().expect("a directory");
        let pid_file = directory.path().join("child");
        let script = format!("sleep 30 & echo $! > '{}'; exit 3", pid_file.display());

        let Supervised { status, .. } =
            supervise(&mut shell(&script), &within(far(), &Interrupt::new()), 0)
                .expect("a failing build is not an error of the supervisor");

        assert_eq!(status.code(), Some(3));
        let child = recorded(&pid_file);
        wait_until(
            || !alive(child),
            "the program that the failed build left running is still running",
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_whole_process_group_of_a_program_is_gone_when_the_supervisor_returns() {
        // The shell prints the id of its child and of its own process group.
        let Supervised { status, lines, .. } = supervise(
            &mut shell("sleep 30 & echo $!; ps -o pgid= -p $$"),
            &within(far(), &Interrupt::new()),
            KEPT_LINES,
        )
        .expect("the build ends");

        let [child, group] = lines.as_slice() else {
            panic!("the id of the child and of the group: {lines:?}");
        };
        let child: u32 = child.trim().parse().expect("a process id");
        let group: u32 = group.trim().parse().expect("a process group id");
        assert!(status.success());
        wait_until(
            || !excise_harness::safety::process_group_exists(group),
            "something is still in the process group of the program",
        );
        assert!(!alive(child));
    }

    #[cfg(unix)]
    #[test]
    fn the_deadline_and_the_interrupt_end_what_a_build_started_with_the_build() {
        let directory = tempfile::tempdir().expect("a directory");
        for (name, by_interrupt) in [("deadline", false), ("interrupt", true)] {
            let pid_file = directory.path().join(name);
            let script = format!("sleep 30 & echo $! > '{}'; wait", pid_file.display());
            let interrupt = Interrupt::new();
            let trigger = interrupt.clone();
            let watched = pid_file.clone();
            let ringer = thread::spawn(move || {
                if by_interrupt {
                    recorded(&watched);
                    trigger.trigger();
                }
            });
            let deadline = if by_interrupt {
                far()
            } else {
                Instant::now() + Duration::from_secs(2)
            };

            let error = supervise(&mut shell(&script), &within(deadline, &interrupt), 0)
                .expect_err("the build is ended");

            ringer.join().expect("the thread ends");
            let child = recorded(&pid_file);
            assert!(
                error.to_string().contains("so nothing was run"),
                "{name}: {error}"
            );
            wait_until(
                || !alive(child),
                "the program the build started is still running",
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_the_kill_does_not_end_is_given_up_on_after_the_grace_and_left_alone() {
        use std::cell::Cell;

        // A real program in a real process group, killed for real. What stands in for a kernel
        // that does not let it go is the question the supervisor asks: it is never seen to be gone.
        let mut process = Process::start(&mut shell("sleep 60"), &within(far(), &Interrupt::new()))
            .expect("a program");
        let asked = Cell::new(0_u32);
        let started = Instant::now();

        let error = process
            .end_within(Duration::from_millis(300), &mut |_| {
                asked.set(asked.get() + 1);
                Ok(None)
            })
            .expect_err("a program that is never seen to be gone is given up on");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(error.to_string().contains("could not be ended"), "{error}");
        assert!(
            started.elapsed() >= Duration::from_millis(300)
                && started.elapsed() < Duration::from_secs(10),
            "it gave up after the grace, and not before or long after: {:?}",
            started.elapsed()
        );
        assert!(asked.get() > 1, "it was asked until the grace was over");
        // Ending it again signals and waits for nothing, so the drop that follows a failed
        // supervision is quick, and so is every other path that ends it twice.
        let again = Instant::now();
        let second = process.end_within(Duration::from_secs(30), &mut |_| panic!("asked again"));
        assert!(second.is_err());
        assert!(
            again.elapsed() < Duration::from_secs(1),
            "{:?}",
            again.elapsed()
        );
        // The program really was killed; reaping what the test made is the test's to do.
        assert!(
            !process
                .child
                .wait()
                .expect("the program was killed")
                .success()
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupt_is_acted_on_as_soon_as_the_program_is_ended_or_given_up_on_and_not_before() {
        let interrupt = Interrupt::new();
        let mut process =
            Process::start(&mut shell("sleep 60"), &within(far(), &interrupt)).expect("a program");
        assert_eq!(
            interrupt.supervised(),
            1,
            "the program is the soak's to end"
        );
        interrupt.trigger();
        // The supervisor looks at the interrupt every few milliseconds, which is how it learns
        // that the program must end. That does not arm the exit.
        assert!(interrupt.is_set());
        assert!(!interrupt.is_acted_on());

        // The kill goes out, and the program is never seen to be gone: it is given up on after the
        // grace. The exit has not been armed at any moment of that, while the program is still
        // the soak's to end.
        let error = process
            .end_within(Duration::from_millis(200), &mut |_| {
                assert!(!interrupt.is_acted_on(), "the program is still to be ended");
                Ok(None)
            })
            .expect_err("a program that is never seen to be gone is given up on");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);

        // Given up on, the program is no longer the soak's to end, and it is let go of at once,
        // while `process` is still there: a build that cannot be waited for any longer must not
        // keep a second press from ending the command.
        assert_eq!(interrupt.supervised(), 0);
        assert!(
            interrupt.is_acted_on(),
            "the program was given up on, and nothing is left to end"
        );

        // Reaping what the test made is the test's to do.
        assert!(
            !process
                .child
                .wait()
                .expect("the program was killed")
                .success()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_program_is_let_go_of_when_it_has_been_ended_and_not_when_the_process_is_dropped() {
        // What the build does next (the rest of its output, the copy of the binary, the removal
        // of its scratch area) is file system work, which can block: the exit must be armed by
        // then, with `process` still held by the supervisor.
        let interrupt = Interrupt::new();
        let mut process =
            Process::start(&mut shell("sleep 60"), &within(far(), &interrupt)).expect("a program");
        interrupt.trigger();
        assert_eq!(interrupt.supervised(), 1);

        process.end().expect("a program that the kill ends");

        assert_eq!(interrupt.supervised(), 0, "the program has been ended");
        assert!(interrupt.is_acted_on(), "and the interrupt acted on");
        drop(process);
    }

    #[cfg(unix)]
    #[test]
    fn a_program_is_not_started_once_the_person_has_asked_to_stop() {
        let directory = tempfile::tempdir().expect("a directory");
        let started = directory.path().join("started");
        let interrupt = Interrupt::new();
        interrupt.trigger();

        let refused = Process::start(
            &mut shell(&format!(": > '{}'", started.display())),
            &within(far(), &interrupt),
        );

        let Err(error) = refused else {
            panic!("a program was started after the person had asked the soak to stop");
        };
        assert!(
            error
                .to_string()
                .contains("the build was interrupted, so nothing was run"),
            "{error}"
        );
        assert!(!started.exists(), "the program was started");
        assert_eq!(interrupt.supervised(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn a_program_that_the_kill_ends_is_reaped_at_once_and_not_after_the_grace() {
        let mut process = Process::start(&mut shell("sleep 60"), &within(far(), &Interrupt::new()))
            .expect("a program");
        let started = Instant::now();

        let status = process.end().expect("a program that the kill ends");

        assert!(!status.success());
        assert!(
            started.elapsed() < REAP_GRACE,
            "it was waited for until the grace was over: {:?}",
            started.elapsed()
        );
        // Ending it again is a no-op that gives the same answer.
        assert!(!process.end().expect("ended already").success());
    }

    #[cfg(unix)]
    #[test]
    fn the_reason_a_program_was_ended_says_when_it_could_not_be_ended() {
        use std::os::unix::process::ExitStatusExt as _;

        let reason =
            || io::Error::other("the build passed the bound on the run, so nothing was run");

        let ended = with_how_it_ended(reason(), Ok(ExitStatus::from_raw(9)));
        let stuck = with_how_it_ended(reason(), Err(not_ended(Duration::from_secs(5))));

        assert_eq!(ended.to_string(), reason().to_string());
        assert!(
            stuck
                .to_string()
                .contains("so nothing was run; and the program could not be ended"),
            "{stuck}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_commit_is_the_one_line_git_prints_and_nothing_else() {
        let interrupt = Interrupt::new();
        let bound = Bound {
            what: LOOKUP,
            ..within(far(), &interrupt)
        };
        let commit = "0123456789abcdef0123456789abcdef01234567";

        let found = head_with(&mut shell(&format!("echo {commit}")), &bound).expect("a commit");

        assert_eq!(found, commit);
        for (script, said) in [
            ("echo not-a-commit", "did not print a commit"),
            (
                "echo 0123456789ABCDEF0123456789ABCDEF01234567",
                "did not print a commit",
            ),
            (
                "echo 0123456789abcdef0123456789abcdef0123456",
                "did not print a commit",
            ),
            (
                "echo 0123456789abcdef0123456789abcdef01234567; echo more",
                "exactly one line",
            ),
            ("true", "exactly one line"),
            (
                "printf 0123456789abcdef0123456789abcdef01234567; \
                 head -c 300000 /dev/zero | tr '\\0' x; echo",
                "did not print a commit",
            ),
            ("echo fatal >&2; exit 128", "failed (exit status: 128)"),
        ] {
            let refused = head_with(&mut shell(script), &bound).expect_err("not a commit");

            assert!(refused.to_string().contains(said), "{script}: {refused}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_lookup_that_does_not_end_is_ended_by_the_bound_and_by_the_interrupt() {
        let started = Instant::now();
        let interrupt = Interrupt::new();
        let trigger = interrupt.clone();
        let ringer = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            trigger.trigger();
        });
        let never = Interrupt::new();

        let by_bound = head_with(
            &mut shell("sleep 30 & wait"),
            &Bound {
                what: LOOKUP,
                ..within(started + Duration::from_millis(300), &never)
            },
        )
        .expect_err("the bound passes");
        let by_interrupt = head_with(
            &mut shell("sleep 30 & wait"),
            &Bound {
                what: LOOKUP,
                ..within(far(), &interrupt)
            },
        )
        .expect_err("interrupted");

        ringer.join().expect("the thread ends");
        assert!(
            by_bound
                .to_string()
                .contains("the lookup of HEAD passed the bound on the run, so nothing was run"),
            "{by_bound}"
        );
        assert!(
            by_interrupt
                .to_string()
                .contains("the lookup of HEAD was interrupted, so nothing was run"),
            "{by_interrupt}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the supervisor waited for the lookup"
        );
    }
}
