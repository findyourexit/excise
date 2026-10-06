//! `cargo xtask soak`: a read-only soak of a real tree, which only a person runs.
//!
//! This is a thin wrapper. The soak, its root type, its key allowlist, and its documents live in
//! `excise_harness::soak`; this file reads the command line, makes sure that a person is at a
//! terminal and has typed the root's path, builds the release binary of this checkout
//! (`soak_build`), and turns the outcome into an exit status: 0 once the rounds have run, whatever
//! the metrics say, and non-zero for a refused start, a build that failed or was stopped, a harness
//! error, an interrupted run, or a scratch area that could not be removed.
//!
//! It runs on macOS and Linux, and on no other system. Anywhere else it refuses before it reads the
//! command line (`unsupported_here`, fed by `SUPPORTED`, which is exactly those two operating
//! systems and not every Unix): it has to stop what it starts, with an interrupt handler and by
//! ending a program's whole process group, and Windows has neither, and no other system has been
//! checked to, so a program the soak started could outlive it there.
//!
//! # Before the confirmation
//!
//! From the moment this command runs, it builds nothing and starts nothing, `git` included, until
//! standard input and standard output are terminals and the person has typed the root's canonical
//! path at the prompt: no terminal, and a mistyped path, start no `git`, no build of `excise`, and
//! no scan. That is what is guaranteed, and it is about this command, not about the `cargo xtask
//! soak` that a person types: `cargo xtask` is an alias of `cargo run --locked --package xtask --`
//! (`.cargo/config.toml`), so cargo builds this command and starts it before it can read the
//! terminal or the prompt. That writes the checkout's target directory and runs the build scripts
//! and the procedural macros of this command's dependencies. It is true of every `cargo xtask`
//! command, the soak cannot change it, and the prompt says so; the tests of the command start the
//! built binary directly, which is the part that is the command's own. A program that does not
//! answer the prompt cannot start the soak, and agents must not run it (`AGENTS.md`): the tests of
//! the soak run on fixtures and scratch trees through the library.
//!
//! Before the prompt it refuses an output directory (`excise-soak` in the target directory) that
//! the soak could not write a run in, by the library's own check (`check_output_dir`, which looks
//! at the directory itself and never follows a link): a symbolic link, because the soak writes its
//! files there and what a link leads to can change; something that is not a directory; and a
//! `latest` in it that no run made, which the soak would not replace. Each would otherwise be found
//! only after the build, which would have been wasted.
//! A root that lies inside the target directory or cargo's home directory is refused as well: the
//! build writes there, and nothing says which files, so the root could not be promised unchanged.
//! So is a root that holds the directory the soak makes its scratch areas in (`work_base`): the
//! scan would see the soak's own files, and the build, which makes a scratch area of its own in
//! that directory for its temporary files, would write them in the root. `run_soak` refuses that
//! root too, but only after the build. So is a scratch directory that is not a directory: the soak
//! would fail on it, after a build that was wasted. The scratch directory is resolved once,
//! against the directory the command runs in and with every link followed, to one absolute
//! canonical path, and that path is what is judged against the root and what the build and the soak
//! are given: a relative `EXCISE_E2E_TMPDIR` names one directory to a check that runs where the
//! command does, and another to a build that runs in the checkout.
//! A scratch directory that other users can change is refused as well (`check_private_directory`:
//! it and every directory above it must be on a file system that enforces ownership, be owned by
//! the person or by root, and not be writable by a group or by everybody unless it is sticky; on
//! macOS a volume mounted with "Ignore ownership" is refused, because every user is treated as
//! the owner there). The soak runs a copy of the program under test from it, by path, once for
//! each scan and session, so a user who can rename what is in it could put another program there
//! and have it run on the tree. `run_soak` asks the same question again, and looks at the copy
//! before each scan and session. The target directory and the output directory are asked too, and
//! a note before the prompt is all they get: another user who can change them could replace what
//! cargo builds, but a checkout is often group-writable.
//! And it works out where each place that is written resolves now, links followed, so that the
//! prompt judges the real destination and not its spelling: the target directory, the output
//! directory, and cargo's home directory (`CARGO_HOME`, else `$HOME/.cargo`). Each that lies inside
//! the root is named at the prompt, with what is written there. The build and the soak are given
//! those same resolved places (`Plan`): the build builds in the resolved target directory and has
//! `CARGO_HOME` set to the resolved cargo home, and the output directory is below the resolved
//! target directory, so a link on the way to one of them that is retargeted after the prompt is not
//! followed. Every path the command prints goes through `safe_path_text`, because
//! `CARGO_TARGET_DIR` is the person's own setting and a name in a path can hold a line break, an
//! escape sequence, or a bidirectional control; only the root is shown as it is, because a
//! `SoakRoot` can hold no such character and the person types it back. What the harness says in an
//! error is escaped the same way (`escaped`), because it can name a path that is spelled as it is.
//!
//! # What runs
//!
//! The release build of this checkout, always, and the prompt says so. No environment variable
//! replaces the binary: `EXCISE_E2E_BINARY`, which the other commands honor and which `AGENTS.md`
//! tells people to export, is not read here, because one left in a shell would run another program
//! on a real tree. The build is made by the program that `CARGO` names, which cargo sets, to
//! itself, for every program it runs, `cargo xtask` included, whatever the shell had; the tests of
//! this command stand a script in for the build through that variable.
//!
//! After the confirmation, and not before, the command installs its interrupt handler and starts
//! the thread that arms the exit of a second press (`handle_signals`), starts the clock of the
//! bound on the whole run, looks up `HEAD` (`git rev-parse HEAD`, run in the checkout
//! that is built, with no variable of git's own inherited, and under the same supervision as the
//! build), says which commit it builds, makes a scratch area for the build in the scratch
//! directory, and builds. The clock counts from the instant at which `confirm` read the line that
//! confirmed the root, and it is one deadline from there to the end: the soak is given that
//! instant, and not what is left of the bound. The build is `cargo build --release --locked
//! --offline`, told where to build (`--target-dir`, the resolved directory the prompt names) and
//! to report what it made, with `CARGO_HOME` set to the cargo home that the prompt classified; the
//! binary that runs is a private copy, made in the scratch area under the same bound, of the one
//! cargo reports for this build: never a path guessed from the target directory, and never the path
//! cargo reported, which another build in the same target directory can replace while the soak
//! runs. The soak is handed the copy's identity with its path (`soak_build::Built`), and runs it
//! only if the file that it opens is that one, unchanged. `soak_build` has the detail, and what
//! the build is isolated from, and what it is not. The build counts against the bound, it is ended
//! with its whole process group when Ctrl+C, Ctrl+\, Ctrl+Z, a termination request, a hang-up, or
//! the bound comes, and so is whatever it started in that group and left running when it ends by
//! itself, whether it succeeded or not. A program that a build script moves to another session or
//! process group is not in the group, and is not ended. A second Ctrl+C or Ctrl+\ ends the command
//! at once only when the exit is armed (`install_interrupt_handler`): the person has asked to
//! stop and no program is left to end, because the one it was running has been ended with its
//! process group and waited for, or given up on (the last one let go of arms the exit at once, and
//! a thread of its own arms it within 25 ms when none was held and the thread that runs the soak
//! is blocked in the file system). A command that ended before would leave the program running
//! without the bounds of the run. The scans get what time is left, counted from the same instant.
//! Nothing else is started and nothing is written in the output directory after a build that was
//! stopped. The scratch area of the build is removed explicitly, at the end of the run and on every
//! way out, and a removal that fails is an error of the command: it is named after the report is
//! printed, and the exit is non-zero.
//!
//! # What it writes
//!
//! Cargo and the soak write nothing in the root, with exceptions that the prompt names before it
//! asks for the root's path: the places below that lie inside it (`cargo xtask soak ~` from a
//! checkout under the home directory has two of them). The target directory: cargo's build
//! replaces its own build files there, and the soak keeps its files in `excise-soak/` below it,
//! new files and the link `latest`. Cargo's home directory: even an offline build updates cargo's
//! usage database and takes its lock files, and unpacks a crate that was downloaded and not yet
//! used; with `--offline` and cargo's cache cleaning off it downloads nothing and cleans nothing.
//! The scratch directory, which has been judged to lie outside the root, gets the scratch area of
//! the build and the soak's own. What the code that the build runs writes (build scripts,
//! procedural macros, a configured linker, `RUSTC`, `RUSTFLAGS`, `[env]` entries, all of them
//! named by the checkout and by the person's own cargo configuration, and run with the person's
//! rights) is not confined, and the prompt says so.

use std::{
    env,
    error::Error,
    ffi::OsString,
    fmt, fs,
    io::{self, BufRead, IsTerminal as _, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[cfg(unix)]
use excise_harness::soak::Armer;
use excise_harness::{
    report::{SoakOutcome, SoakRounds},
    runner::work_base,
    safety::{Scratch, ScratchError, Untrusted, check_private_directory},
    soak::{
        DEFAULT_ROUNDS, DEFAULT_RUN_LIMIT, Interrupt, Limits, SoakReport, SoakRequest, SoakRoot,
        check_output_dir, resolve_through_ancestors, run_soak, safe_path_text,
    },
    tui::parse_duration,
};

use crate::soak_build::{self, Build};

const USAGE: &str = "usage: cargo xtask soak <ROOT> [--rounds N] [--timeout DURATION] [--record]
  <ROOT>      the directory to soak, read-only; a person runs this command, at a terminal, and types
              the root's full path to confirm it (agents must not run it)
  --rounds N  rounds of a headless scan and a session in a terminal under each profile (default 1)
  --timeout   the bound on the whole run, counted from your confirmation and the build included:
              250ms, 30s, 15m, or 2h (default 30m)
  --record    keep an asciicast of each session (its screens show real names)
it builds this checkout's release binary, offline, and runs that, and nothing else; macOS and
Linux only";

/// The directory below the target directory where the soak keeps its files.
const OUTPUT_DIRECTORY: &str = "excise-soak";

/// Whether this is a system the soak is made for: macOS and Linux, and not every Unix. The
/// interrupt handler and the kill of a program's whole process group exist on other Unix systems
/// too, but nothing has checked that the soak stops what it starts there.
const SUPPORTED: bool = cfg!(any(target_os = "macos", target_os = "linux"));

/// Why the soak refuses to run anywhere else.
const UNSUPPORTED: &str = "\
    cargo xtask soak runs on macOS and Linux only. It has to stop everything it starts\n\
    when you press Ctrl+C or its bound passes, by ending each program's whole process\n\
    group, and it is only known to do that on those two: Windows has no interrupt handler\n\
    here and cannot end a program's whole process group, and on other systems it has not\n\
    been checked, so a program it started could outlive it. The other harness commands\n\
    (e2e, headless, compare) are not affected.";

/// What the prompt says about when this command starts anything. It is said from where the
/// command stands: cargo ran it, and had built and started it before it could ask.
const NOTHING_STARTED: &str = "\
    Until you type the path below, this command starts nothing: no git, no build of\n\
    excise, and no scan. (Cargo ran it as `cargo xtask`, which builds and starts it before\n\
    it can ask; every `cargo xtask` command works that way, and the soak cannot change it.)";

/// What the prompt says is written in the target directory, when that lies inside the root.
const TARGET_WRITES: &str = "\
    cargo builds the release binary here, which replaces its own build files, and makes any\n\
    folder above it that is not there yet. The soak keeps its files in excise-soak/ below it:\n\
    new files, and the link `latest`, which is moved to the newest run. It changes nothing\n\
    else there.";

/// What the prompt says is written in the output directory, when that lies inside the root and the
/// target directory does not.
const OUTPUT_WRITES: &str = "\
    the soak keeps its files here: new files, and the link `latest`, which is\n\
    moved to the newest run. It changes nothing else there.";

/// What the prompt says is written in cargo's home directory, when that lies inside the root.
const CARGO_HOME_WRITES: &str = "\
    cargo writes its own files here: its usage database (.global-cache), its lock files\n\
    (.package-cache and .package-cache-mutate), and it unpacks the source of any crate it\n\
    had downloaded and not yet unpacked. The build is offline and cargo's cache\n\
    cleaning is off for it, so cargo downloads nothing and cleans nothing.";

/// What the prompt says about the code that the build runs: it is the person's own, it runs with
/// their rights, and the soak cannot confine what it writes. What the soak does turn off is
/// listed, and so is what that does not reach.
const BUILD_IS_NOT_CONFINED: &str = "\
    The build runs what your checkout and your own cargo configuration name (build\n\
    scripts, procedural macros, a configured linker, RUSTC, RUSTFLAGS, [env] entries)\n\
    with your rights, and the soak cannot confine what that code writes. It turns off\n\
    what it knows how to: compiler wrappers (sccache and the like), a separate build\n\
    directory, TMPDIR, cargo's cache cleaning, and every GIT_* variable for the commit\n\
    lookup. It ends the build's process group when the build ends, but a helper that a\n\
    build script moves to another session or process group is not contained.";

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Arguments {
    root: PathBuf,
    rounds: u32,
    timeout: Duration,
    record: bool,
}

/// What the prompt tells the person besides the root: the places the build and the soak write, each
/// as it resolves now, links followed. The build and the soak are given these same places and not
/// the spellings they were worked out from, so that a link on the way to one of them that is
/// retargeted after the prompt is not followed.
struct Plan<'a> {
    arguments: &'a Arguments,
    /// The checkout's target directory, where cargo builds: the build is told to build here
    /// (`--target-dir`), and the soak's output directory is below it.
    target: &'a Path,
    /// The soak's output directory, `excise-soak` in the target directory.
    output: &'a Path,
    /// Cargo's home directory, where even an offline build updates its usage database: the build
    /// is given it as `CARGO_HOME`.
    cargo_home: &'a Path,
}

/// Runs the command.
pub fn soak(args: impl Iterator<Item = OsString>) -> Result<(), Box<dyn Error>> {
    unsupported_here(SUPPORTED).map_err(io::Error::other)?;
    let arguments =
        parse(args).map_err(|message| io::Error::other(format!("{message}\n{USAGE}")))?;
    let root =
        SoakRoot::open(&arguments.root).map_err(|error| io::Error::other(error.to_string()))?;

    // Nothing is built or started until a person has said, at a terminal, which tree this is.
    require_a_person(io::stdin().is_terminal(), io::stdout().is_terminal())
        .map_err(io::Error::other)?;
    let repo = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    let target =
        env::var_os("CARGO_TARGET_DIR").map_or_else(|| repo.join("target"), |dir| repo.join(dir));
    let output = target.join(OUTPUT_DIRECTORY);
    refuse_an_output_directory_that_cannot_take_a_run(&output).map_err(io::Error::other)?;
    let cargo_home = cargo_home_dir(&repo, env::var_os("CARGO_HOME"), env::var_os("HOME"))
        .map_err(io::Error::other)?;

    // The prompt judges and names where each place resolves now, which follows every link, and
    // the build and the soak are given those places.
    let (resolved_target, resolved_output, resolved_home) = (
        resolve_through_ancestors(&target),
        resolve_through_ancestors(&output),
        resolve_through_ancestors(&cargo_home),
    );
    let plan = Plan {
        arguments: &arguments,
        target: &resolved_target,
        output: &resolved_output,
        cargo_home: &resolved_home,
    };
    // One absolute canonical path: what is judged here is what the build and the soak are given.
    let ambient = env::current_dir().map_err(|error| {
        io::Error::other(format!(
            "cannot tell which directory the command runs in: {error}"
        ))
    })?;
    let work = scratch_directory(&work_base(), &ambient).map_err(io::Error::other)?;
    refuse_a_root_the_build_writes_in(&root, &plan).map_err(io::Error::other)?;
    refuse_scratch_inside_the_root(&root, &work).map_err(io::Error::other)?;
    refuse_a_scratch_directory_that_others_can_change(&work).map_err(io::Error::other)?;
    for note in notes_about_places_that_others_can_change(&plan) {
        say(&mut io::stdout(), &note);
    }
    let Some(confirmed_at) = confirm(
        &root,
        &plan,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
    )?
    else {
        return Err(
            io::Error::other("that is not the root's full path, so nothing was started").into(),
        );
    };
    build_and_soak(&root, &plan, &repo, &work, confirmed_at)
}

/// Everything after the confirmation: the interrupt handler, the commit, the scratch area of the
/// build, the build, and the soak. The bound on the whole run counts from `confirmed_at`, the
/// instant at which `confirm` read the line that confirmed the root, and is one deadline from there
/// to the end: the soak is given the instant and not what is left of the bound. The build is told
/// to build in the target directory of `plan` and given its cargo home, the places that the prompt
/// named. `work` is where the soak makes its scratch areas, and where the build gets one of its
/// own.
fn build_and_soak(
    root: &SoakRoot,
    plan: &Plan<'_>,
    repo: &Path,
    work: &Path,
    confirmed_at: Instant,
) -> Result<(), Box<dyn Error>> {
    // From here Ctrl+C stops the soak where it waits. Not before: a handler at the prompt would
    // make Ctrl+C do nothing while the line is read, and the person's thinking time is not the
    // run's. The thread that arms the second-press exit runs until this function returns, the
    // removal of the build's scratch area included.
    let interrupt = Interrupt::new();
    #[cfg(unix)]
    let _armer = handle_signals(&interrupt)?;
    let arguments = plan.arguments;
    let deadline = deadline_of(confirmed_at, arguments.timeout).map_err(io::Error::other)?;
    // The commit of the checkout that is built, asked of git there, under the bound and the
    // interrupt.
    let head = soak_build::head_commit(repo, deadline, &interrupt)?;
    say(
        &mut io::stderr(),
        &format!(
            "building the release build of this checkout (HEAD {}, plus any uncommitted changes)",
            &head[..12]
        ),
    );
    with_scratch(work, |scratch| {
        let binary = soak_build::build(&Build {
            repo,
            target: plan.target,
            cargo_home: plan.cargo_home,
            scratch,
            deadline,
            interrupt: &interrupt,
        })?;

        let out_root = plan.target.join(OUTPUT_DIRECTORY);
        let mut progress = progress_to(io::stderr());
        // What the build left of the bound is the soak's to use, and the soak counts it from the
        // same instant: when nothing is left, no soak is called.
        ensure_time_is_left(deadline, arguments.timeout)?;
        let report = run_soak(
            &SoakRequest {
                root,
                binary: &binary.path,
                binary_identity: Some(binary.identity),
                out_root: &out_root,
                work_dir: work,
                rounds: arguments.rounds,
                limits: Limits::for_run(arguments.timeout),
                started: confirmed_at,
                record: arguments.record,
                git_sha: &head,
                interrupt: &interrupt,
            },
            &mut progress,
        )
        .map_err(|error| harness_error(&error))?;
        finish_to(&mut io::stdout(), &report)
    })
}

/// Makes a scratch area for the build in `work`, runs `run` with it, and removes the area, on every
/// way out: its temporary files and the private copy of the binary are in it, so that a compiler
/// that was killed with the build leaves nothing behind. It is removed explicitly, and a removal
/// that fails is an error of the command, named after whatever `run` printed (see
/// [`after_removal`]), where a `Drop` would have let it go unseen.
fn with_scratch<T>(
    work: &Path,
    run: impl FnOnce(&Scratch) -> Result<T, Box<dyn Error>>,
) -> Result<T, Box<dyn Error>> {
    let scratch = make_scratch(work)?;
    let outcome = run(&scratch);
    after_removal(outcome, remove_scratch(scratch))
}

/// A scratch area for the build in `work`. When it cannot be made, the refusal names `work` with
/// every character escaped, and says why by the kind of the failure: what the library's error
/// holds besides it spells the path as it is.
fn make_scratch(work: &Path) -> io::Result<Scratch> {
    Scratch::create(work).map_err(|error| {
        io::Error::other(format!(
            "cannot make a scratch area for the build in\n\n    {}\n\n{}",
            safe_path_text(work),
            cause(&error, "it could not be made")
        ))
    })
}

/// Removes the scratch area of the build, with whatever the build left in it, and says why when it
/// cannot: the refusal names the area with every character escaped, and says why by the kind of
/// the failure. What could not be removed is still there, and it is the person's to remove.
fn remove_scratch(scratch: Scratch) -> io::Result<()> {
    let area = scratch.root().to_path_buf();
    scratch.close().map_err(|error| {
        io::Error::other(format!(
            "the scratch area of the build,\n\n    {}\n\ncould not be removed ({}): what is left in \
             it is yours to remove",
            safe_path_text(&area),
            cause(&error, "it could not be removed")
        ))
    })
}

/// Why a scratch area could not be made or removed, by the kind of the failure, or `otherwise`
/// when the error holds no failure of the system.
fn cause(error: &ScratchError, otherwise: &str) -> String {
    error
        .source()
        .and_then(|source| source.downcast_ref::<io::Error>())
        .map_or_else(|| otherwise.to_owned(), |source| source.kind().to_string())
}

/// What the command ends with when the run ended as `outcome` says and the removal of the build's
/// scratch area ended as `removed` says: the run's own result, unless the area could not be
/// removed. That is an error of its own after a run that went well, and is added to the run's
/// error after one that did not, so that neither is lost.
fn after_removal<T>(
    outcome: Result<T, Box<dyn Error>>,
    removed: io::Result<()>,
) -> Result<T, Box<dyn Error>> {
    match (outcome, removed) {
        (outcome, Ok(())) => outcome,
        (Ok(_), Err(removal)) => Err(removal.into()),
        (Err(error), Err(removal)) => {
            Err(io::Error::other(format!("{error}\n\nand {removal}")).into())
        }
    }
}

/// Refuses when nothing is left of the bound on the run at `deadline`, which `timeout` was counted
/// to: no soak is called then, because it would refuse a run that may take no time.
fn ensure_time_is_left(deadline: Instant, timeout: Duration) -> io::Result<()> {
    if Instant::now() >= deadline {
        return Err(io::Error::other(format!(
            "the build used the whole bound on the run, {timeout:?}, so no scan was started"
        )));
    }
    Ok(())
}

/// Says what the soak found to `out`, and turns how it ended into the exit status: the report is
/// written whatever the outcome, and the error, if there is one, is the caller's to print after it.
fn finish_to(out: &mut impl Write, report: &SoakReport) -> Result<(), Box<dyn Error>> {
    say(out, report.table().trim_end());
    say(
        out,
        &format!(
            "summary.json (metrics and counts, no path or name: the one to share) and quirks.txt\n\
             (local only: it can name things in the tree) are in\n\n    {}",
            safe_path_text(&report.run_dir)
        ),
    );
    match report.outcome() {
        SoakOutcome::Finished => Ok(()),
        SoakOutcome::Interrupted => {
            Err(io::Error::other(interruption(report.document.rounds)).into())
        }
        SoakOutcome::Failed => Err(io::Error::other(format!(
            "the soak could not go on: {}",
            escaped(report.failure.as_deref().unwrap_or("a harness error"))
        ))
        .into()),
    }
}

/// What the command says of an interrupted run: where it was, told by the rounds that ran to their
/// end. A run with every round counted was interrupted after its last round, and only the saving of
/// the recordings comes after it: that copy was cut short, so the recordings after the cut are not
/// saved, and the one that was cut is kept as far as it got.
fn interruption(rounds: SoakRounds) -> &'static str {
    if rounds.completed == rounds.requested {
        "the soak was interrupted after its last round, while it saved its recordings; what \
         finished is recorded, and a recording that was cut short is kept as far as it got"
    } else {
        "the soak was interrupted before its last round ended; what finished is recorded"
    }
}

/// The error of the command for something that the harness said (`error`): its text, escaped as
/// [`escaped`] does.
fn harness_error(error: &impl fmt::Display) -> io::Error {
    io::Error::other(escaped(&error.to_string()))
}

/// `text`, which the harness wrote, with every control character and bidirectional control in it
/// escaped, as [`safe_path_text`] escapes a path: some of what the harness says spells a path as it
/// is (a scratch area, a program that cannot be started, a recording), and a name in a path can
/// hold a line break, an escape sequence, or a bidirectional control that would rewrite the
/// terminal this is printed on. A backslash is written doubled, so a text that was escaped
/// already shows its escapes doubled, which is fine for a message that is read once.
fn escaped(text: &str) -> String {
    safe_path_text(Path::new(text))
}

fn parse(mut args: impl Iterator<Item = OsString>) -> Result<Arguments, String> {
    let mut root: Option<PathBuf> = None;
    let mut rounds = DEFAULT_ROUNDS;
    let mut timeout = DEFAULT_RUN_LIMIT;
    let mut record = false;
    while let Some(argument) = args.next() {
        let text = argument.to_str().map(str::to_owned);
        match text.as_deref() {
            Some("--rounds") => {
                let text = value(&mut args, "--rounds")?;
                rounds = text
                    .parse()
                    .ok()
                    .filter(|rounds| *rounds > 0)
                    .ok_or_else(|| {
                        format!("`--rounds` takes a positive whole number, not `{text}`")
                    })?;
            }
            Some("--timeout") => {
                let text = value(&mut args, "--timeout")?;
                timeout = parse_duration(&text)
                    .ok()
                    .filter(|timeout| !timeout.is_zero())
                    .ok_or_else(|| {
                        format!(
                            "`--timeout` takes a duration such as 30s, 15m, or 2h, not `{text}`"
                        )
                    })?;
            }
            Some("--record") => record = true,
            Some(flag) if flag.starts_with("--") => {
                return Err(format!("unknown argument `{flag}`"));
            }
            _ => {
                if root.replace(PathBuf::from(argument)).is_some() {
                    return Err("a soak takes one root".to_owned());
                }
            }
        }
    }
    let root = root.ok_or_else(|| "a soak needs the root to soak".to_owned())?;
    Ok(Arguments {
        root,
        rounds,
        timeout,
        record,
    })
}

fn value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<String, String> {
    args.next()
        .and_then(|value| value.into_string().ok())
        .ok_or_else(|| format!("`{flag}` needs a value"))
}

/// Refuses where the soak is not made to run. It has to stop what it starts, with an interrupt
/// handler for Ctrl+C, Ctrl+\, Ctrl+Z, a termination request, and a hang-up, and by ending a
/// program's whole process group, and that is made for macOS and Linux: Windows has neither, and no
/// other system has been checked.
fn unsupported_here(supported: bool) -> Result<(), String> {
    if supported {
        Ok(())
    } else {
        Err(UNSUPPORTED.to_owned())
    }
}

/// Refuses unless a person could be reading and typing: both ends are terminals. A pipe, or a
/// shell with no terminal, is not one.
fn require_a_person(stdin_is_a_terminal: bool, stdout_is_a_terminal: bool) -> Result<(), String> {
    let missing = match (stdin_is_a_terminal, stdout_is_a_terminal) {
        (true, true) => return Ok(()),
        (false, true) => "standard input is not a terminal",
        (true, false) => "standard output is not a terminal",
        (false, false) => "neither standard input nor standard output is a terminal",
    };
    Err(format!(
        "cargo xtask soak is for a person at a terminal, and {missing}. It reads a whole tree and \
         starts only after you type that tree's path, so a program that does not answer the prompt \
         cannot start it, and an agent must not run it (see AGENTS.md)"
    ))
}

/// Refuses an output directory that the soak could not write a run in, before anything is built:
/// the library's own check of its request (`check_output_dir`). It looks at the directory itself
/// and never follows a link, and it refuses a symbolic link (the soak writes its files there, and
/// what a link leads to can change, so the prompt could not say where they land), something that
/// is not a directory, and a `latest` that no run made, which the soak would not replace. A
/// directory that does not exist yet is fine, and so is one that takes a run. The paths in what it
/// says went through `safe_path_text`.
fn refuse_an_output_directory_that_cannot_take_a_run(output: &Path) -> Result<(), String> {
    check_output_dir(output).map_err(|error| {
        format!(
            "the soak's output directory cannot take a run:\n\n    {error}\n\n\
             Nothing was built or started. Move what is in the way, or set CARGO_TARGET_DIR to a\n\
             directory where it is not."
        )
    })
}

/// Refuses a root that lies inside a directory the build writes in: the target directory, where
/// cargo replaces its own build files, and cargo's home directory, where it updates its own files
/// and unpacks the crates it had downloaded. The prompt names such a place when it lies inside the
/// root. A place that holds the root could rewrite what is in it, and nothing says which files, so
/// the soak does not promise the root unchanged there and refuses it.
fn refuse_a_root_the_build_writes_in(root: &SoakRoot, plan: &Plan<'_>) -> Result<(), String> {
    for (place, what, variable) in [
        (
            plan.target,
            "the directory the build is made in",
            "CARGO_TARGET_DIR",
        ),
        (plan.cargo_home, "cargo's home directory", "CARGO_HOME"),
    ] {
        if root.lies_inside(place) {
            return Err(format!(
                "the root lies inside {what}:\n\n    {}\n\n\
                 and the build writes there, so it could change what is in the root. Soak a\n\
                 directory outside it, or move it with {variable}.",
                safe_path_text(place)
            ));
        }
    }
    Ok(())
}

/// Refuses a root that holds the directory the soak makes its scratch areas in. The scan would see
/// the soak's own files, and the build, which makes a scratch area of its own there for its
/// temporary files, would write them in the root. `run_soak` refuses it too, but only after the
/// build.
fn refuse_scratch_inside_the_root(root: &SoakRoot, work: &Path) -> Result<(), String> {
    if root.contains(work) {
        return Err(format!(
            "the scratch directory\n\n    {}\n\nis inside the root, so the scan would see the soak's \
             own files and the build would write its\n\
             temporary files in the root. Point EXCISE_E2E_TMPDIR at a directory outside it.",
            safe_path_text(work)
        ));
    }
    Ok(())
}

/// Refuses a scratch directory that another user can change, before anything is built or started:
/// the soak runs a copy of the program under test from it, by path, once for each scan and
/// session, so a user who can rename what is in it could run another program on the tree. The
/// library's own check ([`check_private_directory`]) is asked of the directory, which has been
/// resolved to one canonical path: it and every directory above it must be on a file system that
/// enforces ownership, be owned by the person or by root, and not be writable by a group or by
/// everybody unless it is sticky. The refusal says what to do about the reason it found
/// ([`excise_harness::safety::UntrustedDirectory::way_out`]). `run_soak` asks again.
fn refuse_a_scratch_directory_that_others_can_change(work: &Path) -> Result<(), String> {
    check_private_directory(work).map_err(|error| {
        format!(
            "the scratch directory is not private to you:\n\n    {error}\n\n\
             The soak runs a copy of the program under test from it, once for each scan and\n\
             session, so a user who can change what is in it could run another program on the\n\
             tree. Nothing was built or started. To go on, {}.",
            error.way_out()
        )
    })
}

/// What the person is told, before the prompt, when another user can change the directory the
/// build is made in or the soak's output directory: a note and no refusal. Another user who can
/// change the first could replace what cargo builds before the soak copies it, but a checkout is
/// often writable by its group, so it is for the person to judge, and only the scratch directory
/// must be private. At most one note: the output directory is below the target directory, and a
/// directory above both would be named twice.
fn notes_about_places_that_others_can_change(plan: &Plan<'_>) -> Vec<String> {
    [
        ("the directory the build is made in", plan.target),
        ("the soak's output directory", plan.output),
    ]
    .into_iter()
    .find_map(|(what, place)| {
        check_private_directory(place).err().map(|error| {
            // A mode does nothing on a volume that ignores ownership.
            let remedy = if error.why == Untrusted::IgnoresOwnership {
                "use a checkout on a volume that enforces ownership (a mode changes nothing on \
                 one that ignores it)"
            } else {
                "make the directory private (`chmod go-w`)"
            };
            format!(
                "note: {what} is not private to you:\n\n    {error}\n\n\
                 Another user could replace what cargo builds there before the soak copies it,\n\
                 and the soak would then run that program on your tree. The soak goes on, since\n\
                 a group that has only you in it (the default on some systems) is no risk; if\n\
                 that is not so, {remedy} and start again.\n"
            )
        })
    })
    .into_iter()
    .collect()
}

/// The directory the soak makes its scratch areas in, as one absolute canonical path, or why it
/// cannot be one. `given` is resolved against `ambient`, the directory the command runs in (an
/// absolute `given` stays what it is), every link is followed, and the result must be a
/// directory: the soak makes its scratch areas and the copy of the binary in it, and
/// `EXCISE_E2E_TMPDIR` can name anything. The build runs in the checkout, so a relative spelling
/// that was judged here and handed on as it was would name another directory there.
fn scratch_directory(given: &Path, ambient: &Path) -> Result<PathBuf, String> {
    let absolute = ambient.join(given);
    if !absolute.is_dir() {
        return Err(format!(
            "the scratch directory\n\n    {}\n\nis not a directory. Create it, or point \
             EXCISE_E2E_TMPDIR at one.",
            safe_path_text(&absolute)
        ));
    }
    fs::canonicalize(&absolute).map_err(|error| {
        format!(
            "cannot resolve the scratch directory\n\n    {}\n\n{error}",
            safe_path_text(&absolute)
        )
    })
}

/// Where cargo keeps its home directory for the build this command makes, as cargo works it out:
/// `CARGO_HOME` when it is set and not empty, and a relative one is relative to the directory
/// cargo runs in, the checkout; otherwise `.cargo` in the home directory.
fn cargo_home_dir(
    repo: &Path,
    cargo_home: Option<OsString>,
    home: Option<OsString>,
) -> Result<PathBuf, String> {
    if let Some(dir) = cargo_home.filter(|dir| !dir.is_empty()) {
        return Ok(repo.join(dir));
    }
    home.filter(|dir| !dir.is_empty())
        .map(|home| Path::new(&home).join(".cargo"))
        .ok_or_else(|| {
            "cannot tell where cargo keeps its files: neither CARGO_HOME nor HOME is set".to_owned()
        })
}

/// When the bound on the run passes, counted from `now`.
fn deadline_of(now: Instant, timeout: Duration) -> Result<Instant, String> {
    now.checked_add(timeout)
        .ok_or_else(|| format!("the bound on the run, {timeout:?}, is too long to count"))
}

/// What the soak will do, and what it writes in the tree, as the prompt says it before it asks for
/// the root's path: what runs and for how long, that nothing is started before the path is typed
/// (from where this command stands: cargo ran it), what the keys are, where cargo and the soak
/// write, and that the code the build runs is not confined.
fn prompt(root: &SoakRoot, plan: &Plan<'_>) -> String {
    let recording = if plan.arguments.record {
        "\nIt keeps an asciicast of each session; its screens show real names.\n"
    } else {
        ""
    };
    format!(
        "excise soak: a read-only soak of\n\n    {root}\n\n\
         It runs the release build of this checkout (HEAD at the time you confirm, with\n\
         any uncommitted changes) on that directory, headless and in a terminal, for\n\
         {rounds} round(s) and at most {timeout:?}, counted from your confirmation: the\n\
         build is part of it. The build is `cargo build --release --locked --offline`, so\n\
         it downloads nothing.\n\n\
         {not_started}\n\n\
         It sends the program only the arrow keys, h j k l, Enter,\n\
         Esc, q, and the y that answers the quit prompt: never Backspace, so it cannot\n\
         delete. It reads the whole tree, which takes a while and loads the machine.\n\
         {recording}\n\
         {writes}\n\n\
         {not_confined}\n\n\
         The soak's files are summary.json (metrics and counts, no path or name: the one\n\
         to share) and quirks.txt (local only: it can name things in the tree). Ctrl+C\n\
         stops it, a build in progress included, and keeps what finished.\n\n\
         Type the directory's full path to start, or anything else to stop: ",
        rounds = plan.arguments.rounds,
        timeout = plan.arguments.timeout,
        not_started = NOTHING_STARTED,
        writes = writes(root, plan),
        not_confined = BUILD_IS_NOT_CONFINED,
    )
}

/// What the prompt says about the places that are written: each one that lies inside the root,
/// judged by where it resolves, with what is written there; or that none does. It speaks of cargo
/// and the soak: what the code that the build runs writes is said apart (`BUILD_IS_NOT_CONFINED`).
fn writes(root: &SoakRoot, plan: &Plan<'_>) -> String {
    let mut inside: Vec<(&Path, &str)> = Vec::new();
    if root.contains(plan.target) {
        inside.push((plan.target, TARGET_WRITES));
    } else if root.contains(plan.output) {
        inside.push((plan.output, OUTPUT_WRITES));
    }
    if root.contains(plan.cargo_home) {
        inside.push((plan.cargo_home, CARGO_HOME_WRITES));
    }
    if inside.is_empty() {
        let target = format!("    {}", safe_path_text(plan.target));
        return format!(
            "Cargo and the soak write nothing in that directory (what the code that the build\n\
             runs writes is another matter: see below). The build and the soak's files are in\n\n\
             {target}\n\n\
             outside it."
        );
    }
    let places = inside
        .iter()
        .map(|(path, what)| format!("    {}\n{}", safe_path_text(path), indented(what)))
        .collect::<Vec<_>>()
        .join("\n\n");
    let how_many = if inside.len() == 1 {
        "in one place only"
    } else {
        "in these places only"
    };
    format!("Cargo and the soak write in that directory {how_many}:\n\n{places}")
}

/// `text` with every line indented under the path it describes.
fn indented(text: &str) -> String {
    text.lines()
        .map(|line| format!("        {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Says what the soak will do and asks for the root's full path. Returns the instant at which it
/// read the line, when the line that was typed is exactly the root's canonical path (the end of the
/// line is the only thing forgiven), and `None` for anything else. The bound on the run counts from
/// that instant, so it is taken as the line is read and not once this has returned.
fn confirm(
    root: &SoakRoot,
    plan: &Plan<'_>,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<Option<Instant>> {
    output.write_all(prompt(root, plan).as_bytes())?;
    output.flush()?;
    let mut line = String::new();
    input.read_line(&mut line)?;
    let read_at = Instant::now();
    let typed = line.strip_suffix('\n').map_or(line.as_str(), |rest| {
        rest.strip_suffix('\r').unwrap_or(rest)
    });
    Ok(root.is_named_by(typed).then_some(read_at))
}

/// Writes `text` and a line break to `out`, and ignores a failure: a terminal that has hung up (a
/// closed window, a SIGHUP) must not stop the command before the soak has written what it found. A
/// hang-up only sets the interrupt flag, as the first Ctrl+C does (`install_interrupt_handler`), so
/// the run ends where it waits and what finished is written.
fn say(out: &mut impl Write, text: &str) {
    let _ = writeln!(out, "{text}");
}

/// A progress callback that writes each line to `out`, indented, and ignores a failure to write.
fn progress_to<W: Write>(mut out: W) -> impl FnMut(&str) {
    move |line| say(&mut out, &format!("  {line}"))
}

/// Installs the signal handlers ([`install_interrupt_handler`]), and starts the thread that arms
/// the exit that a second press ends the command with ([`Interrupt::start_armer`]). The thread
/// that runs the soak cannot be relied on to arm it: when the person asks to stop while no
/// program is held, it can be blocked in the file system (the scratch trust check, the removal of
/// a scratch area, a look at what a program left on a mount that has stopped answering), and then
/// nothing else would arm it and every later press would be swallowed. The thread ends when the
/// guard that this returns is dropped. It starts first, so that a thread that cannot be started
/// leaves no handler installed.
#[cfg(unix)]
fn handle_signals(interrupt: &Interrupt) -> io::Result<Armer> {
    let armer = interrupt.start_armer().map_err(|error| {
        io::Error::other(format!(
            "cannot start the thread that arms the exit of a second Ctrl+C: {error}"
        ))
    })?;
    install_interrupt_handler(interrupt)?;
    Ok(armer)
}

/// Makes Ctrl+C, Ctrl+\, Ctrl+Z, a termination request, and a hang-up stop the soak where it
/// waits, so that the programs it started are killed with their process groups and nothing is left
/// in the scratch area. Only Unix has the handlers, and the command runs only on macOS and Linux.
///
/// Each of them sets the flag. Ctrl+C and Ctrl+\, the two that a person sends by pressing a key,
/// can also end the command at once, as the signal would have if no handler had been installed
/// (Ctrl+\ with the status that a shell reports for it, 131, so that it leaves no core file in the
/// checkout), but not before the exit has been armed ([`Interrupt::acted_on_flag`]): the soak has
/// asked to stop and has no program left to end. The program it was running (the build, a scan, a
/// session) is ended with its process group and waited for, or was given up on after its bounded
/// wait, or none was running; the last program that is let go of arms the exit at once, and the
/// thread of [`handle_signals`] arms it within [`excise_harness::soak::ARM_EVERY`] when none was
/// held. Those programs run in process groups that the signal of a terminal does not reach, so a
/// command that ended before then, on a second press that came before the soak had acted on the
/// first, would leave the program running with nothing to bound it. A run that is stuck where
/// nothing the soak does can end it (a program in uninterruptible I/O that has been given up on, a
/// hung mount) still ends when the person asks again, because the exit has been armed by then: no
/// look of the thread that runs the soak is needed for it. The conditional registrations come
/// before the ones that set the flag, as the handlers run in the order they were registered.
///
/// A hang-up, a termination request, and Ctrl+Z never end the command. One hang-up of a terminal
/// delivers SIGHUP twice, milliseconds apart (the shell resends it to its jobs, and then the
/// kernel sends it to the old foreground group), and tools send SIGTERM in pairs, so a second one
/// is not a second request: ending the command at the second would leave a headless scan, which
/// runs in a process group of its own, scanning to its end with no bound and no cap on its report,
/// and the run would write nothing. A stop (SIGTSTP) is caught for another reason: it would stop
/// every thread of the command, the clock of the run and the watch on the report with them, while
/// the scan, in a process group that a stop of the terminal does not reach, went on. Ctrl+Z stops
/// the run where it waits, as a hang-up does. `cargo xtask soak` runs the command by `exec` (on
/// Unix `cargo run` replaces itself with the binary), so Ctrl+Z reaches only the soak, which ends
/// the run as Ctrl+C does and returns. A launcher that stays in front of the command (`make`,
/// `just`, `sh -c`, a script) is stopped by the terminal as usual, and `fg` resumes it: it ends
/// with the command. SIGTTIN and SIGTTOU are left alone: a background write that a caught SIGTTOU
/// interrupted would be restarted for ever.
#[cfg(unix)]
fn install_interrupt_handler(interrupt: &Interrupt) -> io::Result<()> {
    use signal_hook::consts::signal::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP};

    signal_hook::flag::register_conditional_default(SIGINT, interrupt.acted_on_flag())?;
    // The default action of SIGQUIT is a core dump, which would leave a file in the checkout: the
    // one that ends the command ends it with the status that a shell reports for it instead.
    signal_hook::flag::register_conditional_shutdown(
        SIGQUIT,
        128 + SIGQUIT,
        interrupt.acted_on_flag(),
    )?;
    for signal in [SIGINT, SIGQUIT, SIGTERM, SIGHUP, SIGTSTP] {
        signal_hook::flag::register(signal, interrupt.flag())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, io::Cursor, thread};

    use excise_harness::{
        report::{HarnessSoak, SchemaVersion, SoakKind, SoakLimits},
        soak::{QuirkLog, SoakError},
    };

    use super::*;

    fn parsed(args: &[&str]) -> Result<Arguments, String> {
        parse(args.iter().map(|argument| OsString::from(*argument)))
    }

    fn arguments() -> Arguments {
        Arguments {
            root: PathBuf::from("."),
            rounds: 2,
            timeout: Duration::from_mins(5),
            record: false,
        }
    }

    /// `text` with every run of white space as one space: what a person reads, wherever the lines
    /// of a paragraph were broken.
    fn flat(text: &str) -> String {
        text.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    /// A root, and another directory beside it that is outside the root.
    struct Trees {
        inside: tempfile::TempDir,
        outside: tempfile::TempDir,
        root: SoakRoot,
    }

    impl Trees {
        fn new() -> Self {
            let inside = tempfile::tempdir().expect("a directory");
            let outside = tempfile::tempdir().expect("another directory");
            let root = SoakRoot::open(inside.path()).expect("a root");
            Self {
                inside,
                outside,
                root,
            }
        }

        /// The prompt for a soak of the root that builds in `target` and has cargo's home in
        /// `cargo_home`.
        fn prompt(&self, record: bool, target: &Path, cargo_home: &Path) -> String {
            let mut arguments = arguments();
            arguments.record = record;
            prompt(
                &self.root,
                &Plan {
                    arguments: &arguments,
                    target,
                    output: &target.join(OUTPUT_DIRECTORY),
                    cargo_home,
                },
            )
        }
    }

    #[test]
    fn a_root_is_all_that_is_required_and_the_defaults_are_one_round_for_thirty_minutes() {
        let parsed = parsed(&["/some/root"]).expect("valid");

        assert_eq!(parsed.root, PathBuf::from("/some/root"));
        assert_eq!(parsed.rounds, 1);
        assert_eq!(parsed.timeout, Duration::from_mins(30));
        assert!(!parsed.record);
    }

    #[test]
    fn the_flags_are_read_wherever_they_stand() {
        let parsed =
            parsed(&["--rounds", "3", "/r", "--timeout", "90s", "--record"]).expect("valid");

        assert_eq!(parsed.root, PathBuf::from("/r"));
        assert_eq!(parsed.rounds, 3);
        assert_eq!(parsed.timeout, Duration::from_secs(90));
        assert!(parsed.record);
    }

    #[test]
    fn a_command_line_that_is_not_one_root_and_known_flags_is_refused() {
        for args in [
            &[][..],
            &["--record"],
            &["/a", "/b"],
            &["/a", "--rounds", "0"],
            &["/a", "--rounds", "many"],
            &["/a", "--rounds"],
            &["/a", "--timeout", "0s"],
            &["/a", "--timeout", "soon"],
            &["/a", "--quick"],
            &["/a", "--delete"],
            &["/a", "--binary", "/tmp/excise"],
        ] {
            assert!(parsed(args).is_err(), "{args:?} must be refused");
        }
    }

    #[test]
    fn the_soak_runs_on_macos_and_linux_and_not_on_every_unix() {
        // The gate is the operating system the soak is made for, as `std::env::consts::OS` spells
        // it on the platform this runs on: FreeBSD is a Unix and is not one of the two.
        assert_eq!(SUPPORTED, matches!(env::consts::OS, "macos" | "linux"));
        assert!(unsupported_here(true).is_ok());

        let refusal = unsupported_here(false).expect_err("not on this platform");

        let refusal = flat(&refusal);
        assert!(
            refusal.contains("runs on macOS and Linux only"),
            "{refusal}"
        );
        assert!(
            refusal.contains("no interrupt handler") && refusal.contains("process group"),
            "it says why: {refusal}"
        );
        assert!(
            refusal.contains("(e2e, headless, compare) are not affected"),
            "{refusal}"
        );
    }

    #[test]
    fn a_person_is_a_terminal_on_both_ends() {
        assert!(require_a_person(true, true).is_ok());
        for (stdin, stdout) in [(false, true), (true, false), (false, false)] {
            let refusal = require_a_person(stdin, stdout).expect_err("no person");
            assert!(
                refusal.contains("is for a person at a terminal"),
                "{refusal}"
            );
            assert!(refusal.contains("an agent must not run it"), "{refusal}");
            assert!(
                !refusal.contains("cannot run it"),
                "a program that drives a terminal can answer a prompt: {refusal}"
            );
        }
    }

    #[test]
    fn the_bound_counts_from_a_moment_and_a_bound_too_long_to_count_is_refused() {
        let now = Instant::now();

        assert_eq!(
            deadline_of(now, Duration::from_secs(5)),
            Ok(now + Duration::from_secs(5))
        );
        let refusal = deadline_of(now, Duration::MAX).expect_err("too long to count");
        assert!(refusal.contains("too long to count"), "{refusal}");
    }

    #[test]
    fn a_bound_with_time_left_goes_on_and_one_that_the_build_used_up_is_refused() {
        let timeout = Duration::from_mins(5);

        ensure_time_is_left(Instant::now() + timeout, timeout).expect("time is left");
        let refusal =
            ensure_time_is_left(Instant::now(), timeout).expect_err("the bound has passed");

        assert!(
            refusal
                .to_string()
                .contains("used the whole bound on the run, 300s, so no scan was started"),
            "{refusal}"
        );
    }

    #[test]
    fn cargo_keeps_its_files_where_cargo_home_says_and_otherwise_in_the_home_directory() {
        let repo = Path::new("/work/excise");
        let from = |cargo_home: Option<&str>, home: Option<&str>| {
            cargo_home_dir(
                repo,
                cargo_home.map(OsString::from),
                home.map(OsString::from),
            )
        };

        assert_eq!(
            from(Some("/opt/cargo"), Some("/home/me")),
            Ok(PathBuf::from("/opt/cargo"))
        );
        assert_eq!(
            from(Some("tools/cargo"), Some("/home/me")),
            Ok(repo.join("tools/cargo")),
            "a relative one is relative to the directory cargo runs in"
        );
        for cargo_home in [None, Some("")] {
            assert_eq!(
                from(cargo_home, Some("/home/me")),
                Ok(Path::new("/home/me").join(".cargo")),
                "{cargo_home:?}"
            );
        }
        for home in [None, Some("")] {
            let refusal = from(None, home).expect_err("nothing says where");
            assert!(
                refusal.contains("neither CARGO_HOME nor HOME is set"),
                "{refusal}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_output_directory_that_is_a_symbolic_link_is_refused_and_named() {
        let parent = tempfile::tempdir().expect("a directory");
        let elsewhere = parent.path().join("elsewhere");
        fs::create_dir(&elsewhere).expect("a directory");
        let link = parent.path().join("excise-soak");
        std::os::unix::fs::symlink(&elsewhere, &link).expect("a link");

        let refusal = refuse_an_output_directory_that_cannot_take_a_run(&link).expect_err("a link");

        let refusal = flat(&refusal);
        assert!(refusal.contains("cannot take a run"), "{refusal}");
        assert!(refusal.contains("is a symbolic link"), "{refusal}");
        assert!(
            refusal.contains(&*link.to_string_lossy()),
            "it names the link: {refusal}"
        );
        assert!(
            refusal.contains("Nothing was built or started"),
            "{refusal}"
        );
        assert!(
            fs::read_dir(&elsewhere)
                .expect("where the link leads")
                .next()
                .is_none(),
            "the link was looked at and not followed"
        );
    }

    // Unix only. The command refuses on Windows before it reaches this check, so no Windows run
    // prints what it says, and what it prints is not the path as the test made it there: a path
    // is spelled two ways (an 8.3 short name, and the canonical `\\?\` form), and `safe_path_text`
    // doubles the backslashes of the one it prints.
    #[cfg(unix)]
    #[test]
    fn an_output_directory_that_is_not_a_directory_is_refused_before_a_build_is_wasted_on_it() {
        let parent = tempfile::tempdir().expect("a directory");
        let file = parent.path().join("excise-soak");
        fs::write(&file, b"x").expect("a file");

        let refusal = refuse_an_output_directory_that_cannot_take_a_run(&file).expect_err("a file");

        let refusal = flat(&refusal);
        assert!(refusal.contains("cannot take a run"), "{refusal}");
        assert!(
            refusal.contains("is not a directory") && refusal.contains(&*file.to_string_lossy()),
            "{refusal}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_latest_that_no_run_made_is_refused_and_one_that_a_run_made_is_not() {
        let parent = tempfile::tempdir().expect("a directory");
        let output = parent.path().join("excise-soak");
        fs::create_dir(&output).expect("a directory");
        let latest = output.join("latest");

        // What a run makes there is a link, whatever it leads to: even nothing.
        std::os::unix::fs::symlink("20260101T000000Z-1", &latest).expect("a link");
        let by_a_run = refuse_an_output_directory_that_cannot_take_a_run(&output);
        fs::remove_file(&latest).expect("the link goes");
        // A note of the person's and a directory are not what a run made, and are left alone.
        fs::write(&latest, b"draft").expect("a file");
        let by_a_file =
            refuse_an_output_directory_that_cannot_take_a_run(&output).expect_err("a file");
        fs::remove_file(&latest).expect("the file goes");
        fs::create_dir(&latest).expect("a directory");
        let by_a_directory =
            refuse_an_output_directory_that_cannot_take_a_run(&output).expect_err("a directory");

        assert_eq!(by_a_run, Ok(()));
        for refusal in [by_a_file, by_a_directory] {
            let refusal = flat(&refusal);
            assert!(
                refusal.contains("is not a link that a run made")
                    && refusal.contains(&*latest.to_string_lossy()),
                "{refusal}"
            );
        }
        assert!(
            latest.is_dir(),
            "a directory that is not a run's is not touched"
        );
    }

    #[test]
    fn an_output_directory_that_is_a_directory_or_that_does_not_exist_yet_is_fine() {
        let parent = tempfile::tempdir().expect("a directory");
        let existing = parent.path().join("excise-soak");
        fs::create_dir(&existing).expect("a directory");

        assert_eq!(
            refuse_an_output_directory_that_cannot_take_a_run(&existing),
            Ok(())
        );
        assert_eq!(
            refuse_an_output_directory_that_cannot_take_a_run(&parent.path().join("not-yet")),
            Ok(())
        );
        assert_eq!(
            refuse_an_output_directory_that_cannot_take_a_run(
                &parent.path().join("not-yet/below/that")
            ),
            Ok(())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_in_a_refusal_never_reaches_the_terminal_as_it_is() {
        let parent = tempfile::tempdir().expect("a directory");
        // A line break, a screen clear, a window title, a bell, and a bidirectional override.
        let hostile = "evil\n\u{1b}[2J\u{1b}]0;pwned\u{7}\u{202e}name";
        let file = parent.path().join(hostile);
        fs::write(&file, b"x").expect("a file whose name holds control characters");

        let by_the_output =
            refuse_an_output_directory_that_cannot_take_a_run(&file).expect_err("a file");
        let by_the_scratch = make_scratch(&file)
            .expect_err("a file is not a directory")
            .to_string();

        for refusal in [by_the_output, by_the_scratch] {
            assert!(
                !refusal
                    .chars()
                    .any(|character| (character.is_control() && character != '\n')
                        || character == '\u{202e}'),
                "a control character reaches the terminal: {refusal:?}"
            );
            assert!(refusal.contains("[deceptive] "), "{refusal}");
            assert!(refusal.contains(r"evil\n\x1b[2J\x1b]0;pwned"), "{refusal}");
            assert!(refusal.contains(r"\u{202e}name"), "{refusal}");
        }
    }

    // Unix only, like `an_output_directory_that_is_not_a_directory_...`: no Windows run reaches
    // this check, and its paths are spelled two ways there.
    #[cfg(unix)]
    #[test]
    fn a_root_inside_a_directory_that_the_build_writes_in_is_refused_and_named() {
        let holder = tempfile::tempdir().expect("a directory");
        let tree = holder.path().join("tree");
        fs::create_dir(&tree).expect("a directory");
        let root = SoakRoot::open(&tree).expect("a root");
        let elsewhere = tempfile::tempdir().expect("another directory");
        let arguments = arguments();
        let check = |target: &Path, cargo_home: &Path| {
            refuse_a_root_the_build_writes_in(
                &root,
                &Plan {
                    arguments: &arguments,
                    target,
                    output: &target.join(OUTPUT_DIRECTORY),
                    cargo_home,
                },
            )
        };

        let by_target = check(holder.path(), &elsewhere.path().join(".cargo"))
            .expect_err("the root is inside the target directory");
        let by_home = check(&elsewhere.path().join("target"), holder.path())
            .expect_err("the root is inside cargo's home");

        let by_target = flat(&by_target);
        assert!(
            by_target.contains("the root lies inside the directory the build is made in")
                && by_target.contains("CARGO_TARGET_DIR")
                && by_target.contains(&*holder.path().to_string_lossy()),
            "{by_target}"
        );
        let by_home = flat(&by_home);
        assert!(
            by_home.contains("the root lies inside cargo's home directory")
                && by_home.contains("CARGO_HOME")
                && by_home.contains(&*holder.path().to_string_lossy()),
            "{by_home}"
        );
        // A place inside the root, the root itself, and a place beside it are not refused: the
        // prompt names what is written inside the root, and writes outside it are not its business.
        for (target, cargo_home) in [
            (tree.join("target"), elsewhere.path().join(".cargo")),
            (tree.clone(), tree.join(".cargo")),
            (
                elsewhere.path().join("target"),
                elsewhere.path().join(".cargo"),
            ),
        ] {
            assert_eq!(
                check(&target, &cargo_home),
                Ok(()),
                "{target:?} {cargo_home:?}"
            );
        }
    }

    // Unix only, for the same reason.
    #[cfg(unix)]
    #[test]
    fn a_root_that_holds_the_scratch_directory_is_refused_and_one_that_does_not_is_not() {
        let trees = Trees::new();
        let inside = trees.inside.path().join("scratch");

        let below = refuse_scratch_inside_the_root(&trees.root, &inside)
            .expect_err("the scratch directory is below the root");
        let itself = refuse_scratch_inside_the_root(&trees.root, trees.inside.path())
            .expect_err("the scratch directory is the root");

        let below = flat(&below);
        assert!(
            below.contains("the scratch directory")
                && below.contains("is inside the root")
                && below.contains("EXCISE_E2E_TMPDIR")
                && below.contains(&*inside.to_string_lossy()),
            "{below}"
        );
        assert!(flat(&itself).contains("is inside the root"));
        assert_eq!(
            refuse_scratch_inside_the_root(&trees.root, trees.outside.path()),
            Ok(())
        );
    }

    #[test]
    fn the_scratch_directory_is_one_absolute_canonical_path_whatever_it_is_spelled_like() {
        let parent = tempfile::tempdir().expect("a directory");
        let scratch = parent.path().join("scratch");
        fs::create_dir(&scratch).expect("a directory");
        let canonical = fs::canonicalize(&scratch).expect("the directory exists");
        let nowhere = Path::new("/not/where/the/command/runs");

        // An absolute spelling is its own, wherever the command runs.
        assert_eq!(scratch_directory(&scratch, nowhere), Ok(canonical.clone()));
        // A relative one is a place below the directory the command runs in.
        for spelling in ["scratch", "./scratch", "scratch/../scratch"] {
            assert_eq!(
                scratch_directory(Path::new(spelling), parent.path()),
                Ok(canonical.clone()),
                "{spelling}"
            );
        }
        // And only there: the same spelling, from another directory, is another place.
        assert!(scratch_directory(Path::new("scratch"), nowhere).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_scratch_directory_that_is_a_link_is_the_directory_it_leads_to() {
        let parent = tempfile::tempdir().expect("a directory");
        let real = parent.path().join("real");
        fs::create_dir(&real).expect("a directory");
        let link = parent.path().join("link");
        std::os::unix::fs::symlink(&real, &link).expect("a link");

        assert_eq!(
            scratch_directory(&link, parent.path()),
            Ok(fs::canonicalize(&real).expect("the directory exists"))
        );
    }

    #[test]
    fn a_relative_scratch_directory_is_judged_where_the_build_will_use_it() {
        // The command runs in `sub`, and `EXCISE_E2E_TMPDIR=tmp` is `sub/tmp` there: that is what
        // is judged against the root and what the build gets, and not `tmp` of the checkout.
        let parent = tempfile::tempdir().expect("a directory");
        let sub = parent.path().join("sub");
        fs::create_dir_all(sub.join("tmp")).expect("directories");
        fs::create_dir(parent.path().join("tmp")).expect("a directory");
        let inside_sub = SoakRoot::open(sub.join("tmp")).expect("a root");
        let beside = SoakRoot::open(parent.path().join("tmp")).expect("a root");

        let work = scratch_directory(Path::new("tmp"), &sub).expect("a directory");

        assert_eq!(
            work,
            fs::canonicalize(sub.join("tmp")).expect("the directory exists")
        );
        assert!(refuse_scratch_inside_the_root(&inside_sub, &work).is_err());
        assert_eq!(refuse_scratch_inside_the_root(&beside, &work), Ok(()));
    }

    #[test]
    fn a_scratch_directory_that_is_not_a_directory_is_refused_before_a_build_is_wasted_on_it() {
        let parent = tempfile::tempdir().expect("a directory");
        let file = parent.path().join("a-file");
        fs::write(&file, b"x").expect("a file");

        let missing =
            scratch_directory(Path::new("not-yet"), parent.path()).expect_err("no such directory");
        let not_a_directory = scratch_directory(&file, parent.path()).expect_err("a file");

        assert!(flat(&missing).contains("is not a directory"), "{missing}");
        assert!(flat(&not_a_directory).contains("EXCISE_E2E_TMPDIR"));
        assert!(scratch_directory(parent.path(), parent.path()).is_ok());
    }

    #[test]
    fn only_the_roots_exact_path_confirms_it() {
        let parent = tempfile::tempdir().expect("a directory");
        let root = SoakRoot::open(parent.path()).expect("a root");
        let path = root.canonical_text().to_owned();
        let target = parent.path().join("target");
        let output_dir = target.join(OUTPUT_DIRECTORY);
        let home = parent.path().join(".cargo");
        let arguments = arguments();
        let plan = Plan {
            arguments: &arguments,
            target: &target,
            output: &output_dir,
            cargo_home: &home,
        };

        for typed in [format!("{path}\n"), format!("{path}\r\n"), path.clone()] {
            let mut output = Vec::new();
            let confirmed = confirm(&root, &plan, &mut Cursor::new(typed), &mut output)
                .expect("the prompt is shown");
            assert!(confirmed.is_some());
            let shown = String::from_utf8(output).expect("text");
            assert!(
                shown.contains(&path) && shown.contains("never Backspace"),
                "{shown}"
            );
        }
        for typed in [
            String::new(),
            "\n".to_owned(),
            "y\n".to_owned(),
            format!("{path}/\n"),
            format!(" {path}\n"),
            format!("{path}x\n"),
            "/not/the/root\n".to_owned(),
        ] {
            let confirmed = confirm(
                &root,
                &plan,
                &mut Cursor::new(typed.clone()),
                &mut Vec::new(),
            )
            .expect("the prompt is shown");
            assert!(confirmed.is_none(), "{typed:?} must not confirm the root");
        }
    }

    // Unix only, for the same reason.
    #[cfg(unix)]
    #[test]
    fn the_prompt_says_what_runs_without_a_commit_and_names_the_one_place_it_writes_in_the_root() {
        let trees = Trees::new();
        let inside_target = trees.inside.path().join("excise/target");
        let beside_home = trees.outside.path().join(".cargo");

        let inside = trees.prompt(true, &inside_target, &beside_home);
        let beside = trees.prompt(true, &trees.outside.path().join("target"), &beside_home);

        for shown in [&inside, &beside] {
            let text = flat(shown);
            assert!(
                text.contains("release build of this checkout (HEAD at the time you confirm,"),
                "what runs, and no commit: {text}"
            );
            assert!(text.contains("any uncommitted changes"), "{text}");
            assert!(
                text.contains("`cargo build --release --locked --offline`"),
                "how it is built: {text}"
            );
            assert!(
                text.contains("counted from your confirmation: the build is part of it"),
                "{text}"
            );
            assert!(text.contains("never Backspace"), "{text}");
            assert!(text.contains("asciicast"), "{text}");
            assert!(
                text.contains("Ctrl+C stops it, a build in progress included"),
                "{text}"
            );
            assert!(
                text.ends_with("or anything else to stop:"),
                "the last words are the question: {text}"
            );
        }
        let text = flat(&inside);
        assert!(
            text.contains("in one place only") && text.contains(&*inside_target.to_string_lossy()),
            "a target inside the root is named, as the one place written: {text}"
        );
        assert!(
            text.contains("`latest`"),
            "the replaced link is said: {text}"
        );
        assert!(
            !text.contains(".global-cache") && !text.contains("write nothing"),
            "cargo's home is outside the root: {text}"
        );
        let text = flat(&beside);
        assert!(
            text.contains("write nothing in that directory")
                && text.contains(&*trees.outside.path().join("target").to_string_lossy())
                && !text.contains("in one place only"),
            "a target outside the root is not written in: {text}"
        );
    }

    #[test]
    fn the_prompt_says_what_is_started_before_the_path_is_typed_and_what_cargo_already_did() {
        let trees = Trees::new();

        let shown = flat(&trees.prompt(
            false,
            &trees.outside.path().join("target"),
            &trees.outside.path().join(".cargo"),
        ));

        assert!(
            shown.contains(
                "Until you type the path below, this command starts nothing: no git, no build \
                 of excise, and no scan."
            ),
            "what is guaranteed, from the moment the command runs: {shown}"
        );
        assert!(
            shown.contains(
                "(Cargo ran it as `cargo xtask`, which builds and starts it before it can ask; \
                 every `cargo xtask` command works that way, and the soak cannot change it.)"
            ),
            "what is not guaranteed, and why: {shown}"
        );
    }

    #[test]
    fn the_prompt_says_that_the_code_the_build_runs_is_not_confined_and_what_is_turned_off() {
        let trees = Trees::new();

        let shown = flat(&trees.prompt(
            false,
            &trees.outside.path().join("target"),
            &trees.outside.path().join(".cargo"),
        ));

        assert!(
            shown.contains(
                "The build runs what your checkout and your own cargo configuration name (build \
                 scripts, procedural macros, a configured linker, RUSTC, RUSTFLAGS, [env] \
                 entries) with your rights, and the soak cannot confine what that code writes."
            ),
            "{shown}"
        );
        assert!(
            shown.contains(
                "It turns off what it knows how to: compiler wrappers (sccache and the like), a \
                 separate build directory, TMPDIR, cargo's cache cleaning, and every GIT_* \
                 variable for the commit lookup."
            ),
            "{shown}"
        );
        assert!(
            shown.contains(
                "It ends the build's process group when the build ends, but a helper that a \
                 build script moves to another session or process group is not contained."
            ),
            "{shown}"
        );
        assert!(
            shown.contains(
                "Cargo and the soak write nothing in that directory (what the code that the \
                 build runs writes is another matter: see below)."
            ),
            "what the prompt says is written is what cargo and the soak write: {shown}"
        );
    }

    // Unix only, for the same reason.
    #[cfg(unix)]
    #[test]
    fn cargos_home_is_named_when_it_lies_inside_the_root_and_not_when_it_does_not() {
        let trees = Trees::new();
        let inside_target = trees.inside.path().join("excise/target");
        let outside_target = trees.outside.path().join("target");
        let inside_home = trees.inside.path().join("home/.cargo");
        let outside_home = trees.outside.path().join("home/.cargo");

        let both = flat(&trees.prompt(false, &inside_target, &inside_home));
        let only_home = flat(&trees.prompt(false, &outside_target, &inside_home));
        let neither = flat(&trees.prompt(false, &outside_target, &outside_home));

        assert!(
            both.contains("in these places only")
                && both.contains(&*inside_target.to_string_lossy())
                && both.contains(&*inside_home.to_string_lossy()),
            "{both}"
        );
        assert!(
            only_home.contains("in one place only")
                && only_home.contains(&*inside_home.to_string_lossy())
                && !only_home.contains(&*outside_target.to_string_lossy())
                && !only_home.contains("`latest`"),
            "the soak's own files are outside the root, so only cargo's home is named: {only_home}"
        );
        for named in [&both, &only_home] {
            assert!(
                named.contains(".global-cache")
                    && named.contains(".package-cache")
                    && named.contains("downloads nothing and cleans nothing"),
                "what cargo writes there is said: {named}"
            );
        }
        assert!(
            neither.contains("write nothing in that directory")
                && !neither.contains(&*outside_home.to_string_lossy())
                && !neither.contains(".global-cache"),
            "a cargo home outside the root is not named: {neither}"
        );
    }

    #[test]
    fn an_output_directory_inside_the_root_is_named_even_when_the_target_directory_is_not() {
        // The root is the soak's own output directory: the soak writes its files there, and the
        // target directory, which holds the build, is above it.
        let outside = tempfile::tempdir().expect("a directory");
        let target = outside.path().join("target");
        let output = target.join(OUTPUT_DIRECTORY);
        fs::create_dir_all(&output).expect("directories");
        let root = SoakRoot::open(&output).expect("a root");
        let home = outside.path().join(".cargo");
        let arguments = arguments();

        let shown = flat(&prompt(
            &root,
            &Plan {
                arguments: &arguments,
                target: &target,
                output: &output,
                cargo_home: &home,
            },
        ));

        assert!(
            shown.contains("in one place only")
                && shown.contains("the soak keeps its files here")
                && !shown.contains("cargo builds the release binary here"),
            "{shown}"
        );
    }

    #[test]
    fn what_the_prompt_names_is_escaped_and_holds_no_control_character() {
        let trees = Trees::new();
        // A line break, a screen clear, a window title, a bell, and a bidirectional override.
        let hostile = "evil\n\u{1b}[2J\u{1b}]0;pwned\u{7}\u{202e}name";
        let inside_target = trees.inside.path().join(hostile);
        let inside_home = trees.inside.path().join(hostile).join("home");
        let outside_target = trees.outside.path().join(hostile);

        let inside = trees.prompt(true, &inside_target, &inside_home);
        let outside = trees.prompt(true, &outside_target, &trees.outside.path().join(".cargo"));

        for shown in [&inside, &outside] {
            assert!(
                !shown
                    .chars()
                    .any(|character| (character.is_control() && character != '\n')
                        || character == '\u{202e}'),
                "a control character reaches the terminal: {shown:?}"
            );
            assert!(shown.contains("[deceptive] "), "{shown}");
            assert!(shown.contains(r"evil\n\x1b[2J\x1b]0;pwned"), "{shown}");
            assert!(shown.contains(r"\u{202e}name"), "{shown}");
        }
        assert!(
            inside.contains("in these places only"),
            "the target and cargo's home are both inside the root: {inside}"
        );
    }

    #[test]
    fn cargo_names_itself_for_the_programs_it_runs() {
        // The build is made by the program `CARGO` names, and the tests of the command stand a
        // script in for the build through it. A real run is `cargo xtask`, which cargo runs, so
        // `CARGO` is cargo's own whatever the shell had; this holds for `cargo test` as well.
        assert!(
            env::var_os("CARGO").is_some_and(|cargo| !cargo.is_empty()),
            "cargo did not set CARGO for a program it ran"
        );
    }

    /// A terminal that has hung up: every write fails.
    struct HungUp;

    impl Write for HungUp {
        fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("the terminal hung up"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("the terminal hung up"))
        }
    }

    #[test]
    fn a_terminal_that_has_hung_up_does_not_stop_the_command_from_reporting() {
        let mut progress = progress_to(HungUp);

        progress("round 1/1: headless scan");
        progress("round 1/1: session in a terminal (default)");
        say(&mut HungUp, "a table");
    }

    #[test]
    fn progress_is_written_indented_one_line_at_a_time() {
        let mut written = Vec::new();
        let mut progress = progress_to(&mut written);

        progress("round 1/2: headless scan");
        progress("saving 2 recording(s)");
        drop(progress);

        assert_eq!(
            String::from_utf8(written).expect("text"),
            "  round 1/2: headless scan\n  saving 2 recording(s)\n"
        );
    }

    /// A person who thinks before they press Enter: the first time the line is asked for, it
    /// arrives `delay` later, and the instant at which it did is kept.
    struct Slow<'a> {
        line: Cursor<String>,
        delay: Duration,
        waited: bool,
        arrived: &'a Cell<Option<Instant>>,
    }

    impl io::Read for Slow<'_> {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let length = available.len().min(buffer.len());
            buffer[..length].copy_from_slice(&available[..length]);
            self.consume(length);
            Ok(length)
        }
    }

    impl BufRead for Slow<'_> {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if !self.waited {
                self.waited = true;
                thread::sleep(self.delay);
                self.arrived.set(Some(Instant::now()));
            }
            self.line.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.line.consume(amount);
        }
    }

    #[test]
    fn the_confirmation_is_the_instant_the_line_was_read_and_not_the_one_the_prompt_was_shown() {
        let parent = tempfile::tempdir().expect("a directory");
        let root = SoakRoot::open(parent.path()).expect("a root");
        let target = parent.path().join("target");
        let output_dir = target.join(OUTPUT_DIRECTORY);
        let home = parent.path().join(".cargo");
        let arguments = arguments();
        let plan = Plan {
            arguments: &arguments,
            target: &target,
            output: &output_dir,
            cargo_home: &home,
        };
        let arrived = Cell::new(None);
        let mut input = Slow {
            line: Cursor::new(format!("{}\n", root.canonical_text())),
            delay: Duration::from_millis(300),
            waited: false,
            arrived: &arrived,
        };
        let prompted_at = Instant::now();

        let confirmed = confirm(&root, &plan, &mut input, &mut Vec::new());
        let returned_at = Instant::now();

        let confirmed_at = confirmed
            .expect("the prompt is shown")
            .expect("the root's path was typed");
        let arrived_at = arrived.get().expect("the line was asked for");
        assert!(
            confirmed_at >= prompted_at + Duration::from_millis(300),
            "the bound counts from the Enter that was waited for, and not from the prompt"
        );
        assert!(
            arrived_at <= confirmed_at && confirmed_at <= returned_at,
            "the instant was taken as the line was read, between its arrival and the return"
        );
    }

    /// What a soak that ended as `outcome` reports, with `completed` of `requested` rounds counted
    /// and `failure` as the harness error that ended it.
    fn report(
        outcome: SoakOutcome,
        completed: u32,
        requested: u32,
        failure: Option<&str>,
    ) -> SoakReport {
        SoakReport {
            run_dir: PathBuf::from("/work/target/excise-soak/20260101T000000Z-1"),
            document: HarnessSoak {
                document_kind: SoakKind::HarnessSoak,
                schema_version: SchemaVersion,
                run_id: "20260101T000000Z-1".to_owned(),
                started_at: "2026-01-01T00:00:00Z".to_owned(),
                finished_at: "2026-01-01T00:00:01Z".to_owned(),
                os: "linux".to_owned(),
                arch: "x86_64".to_owned(),
                git_sha: "0".repeat(40),
                excise_sha256: "0".repeat(64),
                outcome,
                rounds: SoakRounds {
                    requested,
                    completed,
                },
                limits: SoakLimits { run_ms: 1000 },
                headless: Vec::new(),
                tui: Vec::new(),
                quirks: Vec::new(),
            },
            quirks: QuirkLog::new(),
            failure: failure.map(str::to_owned),
        }
    }

    #[test]
    fn an_interrupted_run_says_where_it_was_by_the_rounds_that_ran_to_their_end() {
        let mut written = Vec::new();
        let during = finish_to(&mut written, &report(SoakOutcome::Interrupted, 1, 3, None))
            .expect_err("a stop");
        let while_saving = finish_to(
            &mut Vec::new(),
            &report(SoakOutcome::Interrupted, 3, 3, None),
        )
        .expect_err("a stop");

        assert_eq!(
            during.to_string(),
            "the soak was interrupted before its last round ended; what finished is recorded"
        );
        let while_saving = while_saving.to_string();
        assert!(
            while_saving
                .contains("interrupted after its last round, while it saved its recordings"),
            "{while_saving}"
        );
        assert!(
            !while_saving.contains("before its last round"),
            "every round is counted, so none was left to run: {while_saving}"
        );
        assert!(finish_to(&mut Vec::new(), &report(SoakOutcome::Finished, 3, 3, None)).is_ok());
        let written = String::from_utf8(written).expect("text");
        assert!(
            written.contains("summary.json")
                && written.contains("/work/target/excise-soak/20260101T000000Z-1"),
            "what the run found is written, though the run was interrupted: {written}"
        );
    }

    #[test]
    fn what_the_harness_says_of_a_failure_is_escaped_before_it_is_printed() {
        // A line break, a screen clear, a window title, a bell, and a bidirectional override: a
        // scratch area, a program that cannot be started, or a recording is named as it is spelled.
        let hostile = "scratch area `/tmp/evil\n\u{1b}[2J\u{1b}]0;pwned\u{7}\u{202e}name`: no room";

        let failed = finish_to(
            &mut Vec::new(),
            &report(SoakOutcome::Failed, 0, 1, Some(hostile)),
        )
        .expect_err("failed");
        let by_run_soak = harness_error(&SoakError::Request(hostile.to_owned()));

        for shown in [failed.to_string(), by_run_soak.to_string()] {
            assert!(
                !shown
                    .chars()
                    .any(|character| character.is_control() || character == '\u{202e}'),
                "a control character reaches the terminal: {shown:?}"
            );
            assert!(shown.contains("[deceptive] "), "{shown}");
            assert!(shown.contains(r"evil\n\x1b[2J\x1b]0;pwned"), "{shown}");
            assert!(shown.contains(r"\u{202e}name"), "{shown}");
            assert!(shown.contains("no room"), "the rest of it is said: {shown}");
        }
        assert!(
            failed.to_string().starts_with("the soak could not go on: "),
            "{failed}"
        );
    }

    #[test]
    fn text_that_needs_no_escape_is_shown_as_it_is() {
        assert_eq!(
            escaped("scratch area `/tmp/xh-scratch-1`: no room"),
            "scratch area `/tmp/xh-scratch-1`: no room"
        );
    }

    #[test]
    fn a_scratch_area_that_cannot_be_removed_is_an_error_after_a_run_that_went_well() {
        let removal = || Err(io::Error::other("the scratch area of the build was left"));

        let went_well = after_removal(Ok(7), Ok(())).expect("nothing went wrong");
        let removal_only = after_removal(Ok(7), removal()).expect_err("the removal failed");
        let run_only =
            after_removal::<u8>(Err(io::Error::other("the build failed").into()), Ok(()))
                .expect_err("the run failed");
        let both = after_removal::<u8>(Err(io::Error::other("the build failed").into()), removal())
            .expect_err("both failed");

        assert_eq!(went_well, 7);
        assert_eq!(
            removal_only.to_string(),
            "the scratch area of the build was left"
        );
        assert_eq!(run_only.to_string(), "the build failed");
        assert_eq!(
            both.to_string(),
            "the build failed\n\nand the scratch area of the build was left",
            "neither is lost"
        );
    }

    #[test]
    fn the_scratch_area_of_the_build_is_removed_whatever_the_run_returns() {
        let work = tempfile::tempdir().expect("a directory");
        let mut areas = Vec::new();

        let went_well = with_scratch(work.path(), |scratch| {
            areas.push(scratch.root().to_path_buf());
            fs::write(scratch.tmp().join("left-by-a-compiler"), b"x").expect("a file");
            Ok(())
        });
        let failed: Result<(), _> = with_scratch(work.path(), |scratch| {
            areas.push(scratch.root().to_path_buf());
            Err(io::Error::other("the build failed").into())
        });

        assert!(went_well.is_ok());
        assert_eq!(
            failed.expect_err("the run failed").to_string(),
            "the build failed"
        );
        assert_eq!(areas.len(), 2);
        assert!(areas.iter().all(|area| !area.exists()), "{areas:?}");
        assert_eq!(fs::read_dir(work.path()).expect("the directory").count(), 0);
    }

    /// Makes a directory that its owner can write again when it is dropped, so that a test that
    /// fails does not leave a directory that cannot be removed.
    #[cfg(unix)]
    struct Unlocks(PathBuf);

    #[cfg(unix)]
    impl Drop for Unlocks {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;

            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_scratch_area_that_cannot_be_removed_is_named_escaped_and_is_an_error_of_the_command() {
        use std::os::unix::fs::PermissionsExt as _;

        // A process that is root can remove what it likes, and there is nothing to try.
        let probe = tempfile::tempdir().expect("a directory");
        let locked_probe = probe.path().join("locked");
        fs::create_dir(&locked_probe).expect("a directory");
        fs::write(locked_probe.join("file"), b"x").expect("a file");
        fs::set_permissions(&locked_probe, fs::Permissions::from_mode(0o500)).expect("chmod");
        let removable = fs::remove_file(locked_probe.join("file")).is_ok();
        fs::set_permissions(&locked_probe, fs::Permissions::from_mode(0o700)).expect("chmod");
        if removable {
            eprintln!("skipped: a process that is root can remove what it likes");
            return;
        }

        let parent = tempfile::tempdir().expect("a directory");
        // A line break, a screen clear, a window title, a bell, and a bidirectional override.
        let work = parent
            .path()
            .join("evil\n\u{1b}[2J\u{1b}]0;pwned\u{7}\u{202e}name");
        fs::create_dir(&work).expect("a directory whose name holds control characters");
        let mut locked = None;
        let mut area = PathBuf::new();

        let outcome = with_scratch(&work, |scratch| {
            area = scratch.root().to_path_buf();
            let directory = scratch.tmp().join("locked");
            fs::create_dir(&directory).expect("a directory");
            fs::write(directory.join("file"), b"x").expect("a file");
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).expect("chmod");
            locked = Some(Unlocks(directory));
            Ok(())
        });

        let error = outcome.expect_err("the area could not be removed");
        let shown = error.to_string();
        assert!(
            shown.contains("the scratch area of the build,")
                && shown.contains("could not be removed"),
            "{shown}"
        );
        assert!(
            shown.contains("permission denied") && shown.contains("yours to remove"),
            "it says why, and whose it is now: {shown}"
        );
        assert!(
            !shown
                .chars()
                .any(|character| (character.is_control() && character != '\n')
                    || character == '\u{202e}'),
            "a control character reaches the terminal: {shown:?}"
        );
        assert!(
            shown.contains("[deceptive] ") && shown.contains(r"evil\n\x1b[2J\x1b]0;pwned"),
            "{shown}"
        );
        assert!(area.exists(), "what could not be removed is left");
        drop(locked);
        fs::remove_dir_all(&area).expect("clean up the area that was left");
    }

    /// The variable that makes `the_command_with_its_handlers_in_and_nothing_calling_the_interrupt`,
    /// run again as a process of its own, the command that has installed its handlers and whose
    /// main thread is stuck. It names the directory in which that process writes `ready` once the
    /// handlers are in, `flagged` once a signal has set the flag, and `armed` once the exit is
    /// armed.
    #[cfg(unix)]
    const HANDLER_CHILD: &str = "XTASK_SOAK_HANDLER_CHILD";

    /// Not a test: the child of the tests below, which run it as a process of their own. It
    /// starts the handlers and the thread that arms the exit as the command does
    /// (`handle_signals`), and then its main thread is what the main thread of a run is when it is
    /// blocked in the file system (the scratch trust check, the removal of a scratch area, a look
    /// at what a program left on a mount that has stopped answering): it makes no call into the
    /// interrupt, it only reads the two atomics that the handlers store to and read, so that
    /// only a signal can end it. It ends by itself after a minute if the test that started it is
    /// gone.
    ///
    /// When the directory holds a file `program`, the child holds a program that it has not ended
    /// either, as the soak does while it ends a scan or a build: it ends it when a file `release`
    /// appears, and writes `released` then.
    #[cfg(unix)]
    #[test]
    #[ignore = "the child of the signal tests, which run it as a process of their own"]
    fn the_command_with_its_handlers_in_and_nothing_calling_the_interrupt() {
        use std::sync::atomic::Ordering;

        let Some(directory) = env::var_os(HANDLER_CHILD).map(PathBuf::from) else {
            return;
        };
        let interrupt = Interrupt::new();
        let _armer = handle_signals(&interrupt).expect("the handlers are installed");
        let mut program = directory.join("program").exists().then(|| {
            interrupt
                .supervising()
                .expect("a program may start before any signal")
        });
        let tell = |name: &str| fs::write(directory.join(name), name).expect("the parent is told");
        tell("ready");

        let (flag, armed) = (interrupt.flag(), interrupt.acted_on_flag());
        let (mut flagged, mut armed_told) = (false, false);
        let give_up = Instant::now() + Duration::from_mins(1);
        while Instant::now() < give_up {
            if !flagged && flag.load(Ordering::SeqCst) {
                tell("flagged");
                flagged = true;
            }
            if flagged && program.is_some() && directory.join("release").exists() {
                // The program is ended: the guard is let go of.
                program = None;
                tell("released");
            }
            if !armed_told && armed.load(Ordering::SeqCst) {
                tell("armed");
                armed_told = true;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Ends and reaps the child process when it goes out of scope, however the test ends.
    #[cfg(unix)]
    struct Reaped(std::process::Child);

    #[cfg(unix)]
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Whether `path` was made within `bound`.
    #[cfg(unix)]
    fn appears(path: &Path, bound: Duration) -> bool {
        let give_up = Instant::now() + bound;
        while !path.exists() {
            if Instant::now() >= give_up {
                return false;
            }
            thread::sleep(Duration::from_millis(5));
        }
        true
    }

    /// The child of these tests, started, with its handlers in.
    #[cfg(unix)]
    struct Running {
        child: Reaped,
        directory: tempfile::TempDir,
    }

    #[cfg(unix)]
    impl Running {
        /// Starts the child and waits until its handlers are in. It holds a program that it has
        /// not ended when `holding_a_program` says so.
        fn start(holding_a_program: bool) -> Self {
            use std::process::{Command, Stdio};

            let directory = tempfile::tempdir().expect("a directory");
            if holding_a_program {
                fs::write(directory.path().join("program"), b"program").expect("a marker");
            }
            let module = module_path!();
            let test = format!(
                "{}::the_command_with_its_handlers_in_and_nothing_calling_the_interrupt",
                module.strip_prefix("xtask::").unwrap_or(module)
            );
            let child = Reaped(
                Command::new(env::current_exe().expect("the test binary"))
                    .args(["--ignored", "--exact", &test, "--test-threads=1"])
                    .env(HANDLER_CHILD, directory.path())
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("the test binary runs"),
            );
            let running = Self { child, directory };
            assert!(
                running.has_within("ready", Duration::from_secs(30)),
                "the child never installed its handlers"
            );
            running
        }

        /// Sends the child `signal` (a `kill` argument, `-INT`).
        fn send(&self, signal: &str) {
            let sent = std::process::Command::new("kill")
                .args([signal, &self.child.0.id().to_string()])
                .status()
                .expect("kill runs");
            assert!(sent.success(), "{signal}");
        }

        /// Whether the child has written `name` within `bound`.
        fn has_within(&self, name: &str, bound: Duration) -> bool {
            appears(&self.directory.path().join(name), bound)
        }

        /// Whether the child has written `name` by now.
        fn has(&self, name: &str) -> bool {
            self.directory.path().join(name).exists()
        }

        /// Writes `name` in the directory of the child, which it is waiting for.
        fn tell(&self, name: &str) {
            fs::write(self.directory.path().join(name), name).expect("the child is told");
        }

        /// How the child ended, when it did within `wait`.
        fn ended_within(&mut self, wait: Duration) -> Option<std::process::ExitStatus> {
            let until = Instant::now() + wait;
            loop {
                if let Some(status) = self.child.0.try_wait().expect("a status") {
                    return Some(status);
                }
                if Instant::now() >= until {
                    return None;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_second_interrupt_ends_the_command_where_the_first_only_asks_it_to_stop() {
        use std::os::unix::process::ExitStatusExt as _;

        let mut command = Running::start(false);

        command.send("-INT");
        assert!(
            command.has_within("flagged", Duration::from_secs(10)),
            "the first interrupt did not set the flag"
        );
        assert!(
            command.ended_within(Duration::ZERO).is_none(),
            "the first interrupt only asks the command to stop"
        );
        // No program is held, and nothing in the child calls the interrupt, as nothing does in a
        // run whose main thread is blocked in the file system: the thread that arms the exit has
        // to, and it looks every 25 ms.
        assert!(
            command.has_within("armed", Duration::from_secs(10)),
            "nothing armed the exit, so a second interrupt would be swallowed for ever"
        );
        command.send("-INT");

        let ended = command
            .ended_within(Duration::from_secs(10))
            .expect("a second interrupt did not end the command");
        assert_eq!(ended.signal(), Some(2), "killed by the second interrupt");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_interrupt_before_the_program_is_ended_is_the_same_request_and_one_after_ends_the_command()
     {
        use std::os::unix::process::ExitStatusExt as _;

        // The command holds a program that it has not ended, as the soak does while it ends a
        // build, a scan, or a session. Those run in process groups that the signal of a terminal
        // does not reach: a command that ended now would leave the program running, with the
        // bounds of the run gone with the command.
        let mut command = Running::start(true);

        command.send("-INT");
        assert!(
            command.has_within("flagged", Duration::from_secs(10)),
            "the first interrupt did not set the flag"
        );
        command.send("-INT");
        command.send("-INT");
        let early = command.ended_within(Duration::from_millis(1500));
        assert!(
            early.is_none(),
            "an interrupt ended the command while the program it held was still to be ended: {early:?}"
        );
        assert!(
            !command.has("armed"),
            "the thread that arms the exit armed it while a program was held"
        );

        // The program is ended: the guard is let go of, which arms the exit, and the person
        // asking again is asking for the command to end.
        command.tell("release");
        assert!(command.has_within("released", Duration::from_secs(10)));
        assert!(command.has_within("armed", Duration::from_secs(10)));
        command.send("-INT");
        let ended = command
            .ended_within(Duration::from_secs(10))
            .expect("an interrupt after the program was ended did not end the command");
        assert_eq!(ended.signal(), Some(2), "killed by the interrupt");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_quit_signal_ends_the_command_with_the_status_a_shell_reports_for_it() {
        let mut command = Running::start(false);

        command.send("-QUIT");
        assert!(
            command.has_within("flagged", Duration::from_secs(10)),
            "the first quit signal did not set the flag"
        );
        assert!(
            command.ended_within(Duration::ZERO).is_none(),
            "the first quit signal only asks the command to stop"
        );
        assert!(
            command.has_within("armed", Duration::from_secs(10)),
            "nothing armed the exit, so a second quit signal would be swallowed for ever"
        );
        command.send("-QUIT");

        let ended = command
            .ended_within(Duration::from_secs(10))
            .expect("a second quit signal did not end the command");
        // No core dump, which would leave a file in the checkout: an exit with 128 + 3.
        assert_eq!(ended.code(), Some(131), "{ended:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_second_quit_signal_before_the_program_is_ended_is_the_same_request_and_one_after_ends_the_command()
     {
        let mut command = Running::start(true);

        command.send("-QUIT");
        assert!(
            command.has_within("flagged", Duration::from_secs(10)),
            "the first quit signal did not set the flag"
        );
        command.send("-QUIT");
        let early = command.ended_within(Duration::from_millis(1500));
        assert!(
            early.is_none(),
            "a quit signal ended the command while the program it held was still to be ended: {early:?}"
        );
        assert!(!command.has("armed"));

        command.tell("release");
        assert!(command.has_within("released", Duration::from_secs(10)));
        assert!(command.has_within("armed", Duration::from_secs(10)));
        command.send("-QUIT");
        let ended = command
            .ended_within(Duration::from_secs(10))
            .expect("a quit signal after the program was ended did not end the command");
        assert_eq!(ended.code(), Some(131), "{ended:?}");
    }

    #[cfg(unix)]
    #[test]
    fn two_hang_ups_terminations_or_stops_only_ask_the_command_to_stop() {
        // One hang-up of a terminal delivers SIGHUP twice, a few milliseconds apart, and tools send
        // SIGTERM in pairs. Neither is a person asking a second time, and the command that ended at
        // the second would orphan its headless scan, which has a process group of its own. Ctrl+Z
        // is no request to stop the command either: a command that it stopped would stop the clock
        // of the run and the watch on the report with it, while the scan, in a process group of
        // its own that a stop of the terminal does not reach, went on. The exit is armed by the
        // time the second one comes, as it would be for Ctrl+C, and the second one still does not
        // end the command: only the two signals that a person sends by pressing a key again do.
        for signal in ["-HUP", "-TERM", "-TSTP"] {
            let mut command = Running::start(false);

            command.send(signal);
            assert!(
                command.has_within("flagged", Duration::from_secs(5)),
                "{signal}: the signal did not set the flag that stops the run"
            );
            assert!(
                command.has_within("armed", Duration::from_secs(10)),
                "{signal}: the exit was not armed"
            );
            command.send(signal);

            let ended = command.ended_within(Duration::from_millis(1500));
            assert!(
                ended.is_none(),
                "{signal}: the second signal ended the command: {ended:?}"
            );
        }
    }

    /// A directory made below `parent` with exactly `mode`, whatever the umask is.
    #[cfg(unix)]
    fn directory_with_mode(parent: &Path, name: &str, mode: u32) -> PathBuf {
        use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

        let path = parent.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("a directory");
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).expect("its mode");
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_scratch_directory_that_others_can_change_is_refused_and_a_private_or_sticky_one_is_not() {
        let base = tempfile::tempdir().expect("a directory");
        let open = directory_with_mode(base.path(), "open", 0o777);
        let group = directory_with_mode(base.path(), "group", 0o770);
        let sticky = directory_with_mode(base.path(), "sticky", 0o1777);
        let private = directory_with_mode(base.path(), "private", 0o700);

        let by_everybody = refuse_a_scratch_directory_that_others_can_change(&open)
            .expect_err("everybody can write it");
        let by_the_group = refuse_a_scratch_directory_that_others_can_change(&group)
            .expect_err("the group can write it");

        let by_everybody = flat(&by_everybody);
        let canonical = fs::canonicalize(&open).expect("a canonical path");
        assert!(
            by_everybody.contains("the scratch directory is not private to you")
                && by_everybody.contains(&*canonical.to_string_lossy())
                && by_everybody.contains("can be written by everybody and is not sticky")
                && by_everybody.contains("Nothing was built or started")
                && by_everybody.contains("EXCISE_E2E_TMPDIR"),
            "{by_everybody}"
        );
        assert!(
            flat(&by_the_group).contains("can be written by its group and is not sticky"),
            "{by_the_group}"
        );
        // The sticky bit stops another user from renaming what the person made, and a directory
        // that only the person can write needs nothing.
        assert_eq!(
            refuse_a_scratch_directory_that_others_can_change(&sticky),
            Ok(())
        );
        assert_eq!(
            refuse_a_scratch_directory_that_others_can_change(&private),
            Ok(())
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_target_directory_that_others_can_change_gets_a_note_and_no_refusal() {
        let base = tempfile::tempdir().expect("a directory");
        let open = directory_with_mode(base.path(), "open", 0o777);
        let private = directory_with_mode(base.path(), "private", 0o700);
        let arguments = arguments();
        let notes = |target: &Path| {
            notes_about_places_that_others_can_change(&Plan {
                arguments: &arguments,
                target,
                output: &target.join(OUTPUT_DIRECTORY),
                cargo_home: &base.path().join(".cargo"),
            })
        };

        let in_the_open = notes(&open.join("target"));
        let in_the_private = notes(&private.join("target"));

        let [note] = in_the_open.as_slice() else {
            panic!("one note: {in_the_open:?}");
        };
        let note = flat(note);
        assert!(
            note.starts_with("note: the directory the build is made in is not private to you")
                && note.contains("can be written by everybody and is not sticky")
                && note.contains("The soak goes on"),
            "{note}"
        );
        assert!(in_the_private.is_empty(), "{in_the_private:?}");
    }
}
