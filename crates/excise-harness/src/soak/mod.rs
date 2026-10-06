//! The read-only soak: Excise on a real tree, with nothing that could delete it.
//!
//! The maintainer used to validate a change with `cargo run --release ~/`: a demanding real tree,
//! but manual, unrepeatable, and able to delete real data. A soak keeps the real-tree signal and
//! removes the danger. It runs Excise headless and in a terminal on a root a person names, never
//! sends a key that could delete, and records metrics and quirks. It gates nothing: whatever the
//! numbers say, a soak that ran to its end is a soak that succeeded. It is **human-triggered**
//! (`cargo xtask soak` refuses to start unless a person types the root's path at a prompt) and
//! agents never run it; see `AGENTS.md`.
//!
//! # Why a soak cannot delete
//!
//! Three things, each checked by a test, and the second does not depend on the first:
//!
//! 1. **The type of the root.** The root is a [`SoakRoot`], not a
//!    [`FixtureRoot`](crate::safety::FixtureRoot). Every step and protocol of this crate that
//!    deletes or changes a fixture (the `delete` and `fs_mutate` steps, the deletion protocol, the
//!    interactive driver) takes a `FixtureRoot`, which only a directory with the ownership marker
//!    becomes, and a `SoakRoot` is neither one nor convertible into one. That keeps the root out of
//!    every API that takes a `FixtureRoot`, and [`root`] has the compile-time proofs, which are
//!    doctests. It does not seal the path: the root prints its path, and `fixture::remove_tree`
//!    and `write_marker` take a plain `&Path`. What keeps this module's code away from the APIs
//!    that take a path is a test that reads its source (`tests.rs`): the modules it may use, and
//!    the names it may never mention.
//! 2. **The keys.** Only Backspace asks `excise` for a deletion, and every deletion dialog opens on
//!    an input that follows a deletion request, so a session that never sends Backspace cannot
//!    delete. The soak does not rest on that fact. Every key it sends to the program goes
//!    through one function, `Driver::send_input` in `session.rs`, which sends only the
//!    [`Key::ALLOWED`] list (arrows, the Vim letters, Enter, Esc, `q`, and the `y` that answers
//!    the quit prompt) and refuses everything else, Backspace and Ctrl+H by name, before it writes.
//!    [`keys`] says why the list cannot compose a key it does not name. Two writes to the program
//!    are not the soak's and are not keys: the pseudo-terminal layer answers a cursor position
//!    request (`ESC [ 6 n`) with `ESC [ row ; col R` (`PtySession::pump`), and dropping a session,
//!    after its program was killed, closes the terminal with a line feed and an end-of-file.
//!    Neither can form a deletion key, and a test pins the shape of the answer.
//! 3. **The isolation.** The program runs in the environment every harness run has: a scratch
//!    `HOME`, configuration, working directory, temporary directory, and scan store, all outside
//!    the root (the soak refuses a scratch area inside it), with the arguments the root alone.
//!    Never `--disable-delete-confirmation`, never a mouse or a custom key.
//!
//! # What a run does
//!
//! A *round* is one headless scan ([`headless`]: `--format json`, supervised, measured, and its
//! report reduced to `state`, `accounting`, and `summary`, read within the run's bound and
//! stopped by the person's interrupt, and then deleted) and one session in a pseudo-terminal for
//! each of the `default` and `deterministic` profiles ([`session`]: first frame, keys during the
//! scan, the end of the scan whatever badge it ends with (`COMPLETE`, or the label of an
//! uncertain result, which is a quirk), a folder opened and left, the quit). `rounds` of them
//! run in turn.
//!
//! The whole run is bounded ([`Limits`], thirty minutes by default) and so is every phase: a
//! phase that passes its bound ends its program with the program's whole process group and the
//! run goes on; the bound on the whole run, or a person's interrupt ([`Interrupt`]), ends the run
//! as `interrupted`. An error of the harness's own, in the middle of a session or while the
//! session is being ended (the program's output cannot be read, its recording cannot be flushed),
//! ends the run as `failed`.
//!
//! The clock of the whole run starts when [`run_soak`] is called, before the request is looked at,
//! so that what the command has already spent is not given back to the bound. The clock and the
//! interrupt are looked at before each phase starts and where a phase waits, and never after the
//! last phase of the last round: a round is complete when its scan and both its sessions were
//! recorded and none was cut short, and a bound that passes, or an interrupt that comes, a moment
//! after the last of them ended does not undo it. (Saving the recordings, below, still answers to
//! both.) A scan or a session that is about to start when the interrupt comes is not started: the
//! guard that says a program is the soak's to end is refused once the interrupt is set
//! ([`Interrupted`], [`SoakError::NotStarted`]), and the run ends as `interrupted`.
//!
//! What runs is a private copy of the binary under test, made once in a directory of its own
//! outside the root, and removed with the run. A soak can take as long as a person likes, and a
//! build in the same target directory replaces the file at the binary's path; every scan and
//! session starts the program afresh, so they start the copy, and `excise_sha256` is the digest of
//! the bytes that ran. The copy and the digest are made a chunk at a time, and the clock and the
//! interrupt are looked at before each chunk: a run that is stopped there (a bound that a slow
//! build or a slow disk used up, a person who gave up waiting) starts no scan, makes no run
//! directory, and removes the copy, and the call fails with [`SoakError::BoundPassed`] or
//! [`SoakError::Interrupted`], which say which of the two it was, so that the command exits
//! non-zero as for an interrupted run.
//!
//! Where the copy runs from is a directory that only the person can change. It lives in the scratch
//! directory, with every scratch area, and it is started by its path, once for each scan and
//! session, so a scratch directory in which another user can rename entries would let that user put
//! another program where the copy was. [`run_soak`] refuses a scratch directory that is not private
//! ([`SoakError::UntrustedScratch`], by [`check_private_directory`]: the directory
//! and every one above it is on a file system that enforces ownership (a macOS volume mounted with
//! "Ignore ownership" is refused), is owned by the person or by root, and is not writable by its
//! group or by everybody unless it is sticky). Behind that, as a second line, the copy is looked at
//! again before each scan and each session: the file at its path must be the one that was made, by
//! device, inode, owner, mode, and change time, so that a replacement and a write in place are both
//! caught ([`SoakError::ProgramReplaced`]).
//!
//! The same holds for the file that is copied. A caller that made the binary for the soak (the
//! command copies what cargo built into a scratch area of its own) hands over, with its path, what
//! the binary was when it was whole ([`SoakRequest::binary_identity`]): the soak opens it without
//! following a link, runs nothing unless the open file is that one, unchanged, and asks again
//! after it has copied it ([`SoakError::BinaryReplaced`]).
//!
//! # What it writes
//!
//! Under `<out_root>/<run-id>/`, only when the scans are over, and in this order:
//!
//! 1. `<round>-tui-<profile>.cast` for each session, only when asked: the screens show real
//!    names. Each is copied from a temporary file outside the root in chunks of 1 MiB, and the
//!    person's interrupt and the clock of the run (its bound, plus 30 seconds that only saving may
//!    use) are looked at between two chunks. A recording whose copy is cut short is kept as far as
//!    it got (an asciicast is read line by line, so its last line can be broken), the recordings
//!    after it are not saved, and the run is `interrupted`.
//! 2. `quirks.txt`: anything unexpected (an error dialog, an uncertain scan, a non-zero exit, a
//!    time-out, a stall), with its circumstances. **Local only**: its text comes from the screen
//!    and the report, and a real tree's names are in both.
//! 3. `summary.json`, **last**, a [`HarnessSoak`](crate::report::HarnessSoak) document: metrics
//!    and counts, with no path, no name, no host name, and no free text, so that it can be shared.
//!    It says how the run ended once everything else was done, so a run whose copy was cut short
//!    says `interrupted`, and a run directory without a `summary.json` is one that did not end.
//!    It is written whole or not at all: into a private temporary file in the run's directory,
//!    which is flushed to the disk and only then given the name `summary.json`, never in place of
//!    a file that is there, and removed when anything fails (a full disk, an exhausted quota). So
//!    a `summary.json` is always a whole document.
//!
//! Then `<out_root>/latest` is pointed at the run (a link on Unix). It is replaced only when it
//! is what a run made: a link on Unix, and elsewhere, where a run writes its id in a file, a file
//! that holds an id exactly as a run writes it (`20261006T120000Z-4242`: a date and a time that
//! exist, a process id written as a number, and at most one line ending after it), so that a note
//! called `draft-1`, a file that begins with an id and goes on, and a string that only looks like
//! an id are left alone. Anything else is left alone, and a soak refuses to start on it.
//!
//! The directory and its files are private to their owner on Unix.
//!
//! # What it writes in the tree
//!
//! Nothing in the root, with one exception that the caller chooses. `out_root` can lie inside the
//! root (the command's is the checkout's `target/excise-soak`, and the checkout can be under the
//! directory that is soaked), and then the run directory, its files, and `latest` are written
//! there: new entries, and a replaced link. Nothing that existed is changed or removed. The
//! command says so before it asks for the root's path, and the build it makes first writes the
//! same `target` directory. The scratch areas, which hold everything else the program and the scan
//! write, are refused inside the root.
//!
//! `out_root` itself must not be a symbolic link ([`SoakError::OutputIsALink`]): the files would
//! be written where it points, and that is a place nothing said when the root's path was
//! confirmed. It is refused when the request is checked, and again when the directory is made,
//! which is done without following a link at its end; the command also judges the directory by
//! where it resolves ([`resolve_through_ancestors`]), and says so.
//!
//! # Platforms
//!
//! The library builds and its tests run on macOS, Linux, and Windows; `cargo xtask soak`, the
//! command, refuses anywhere but macOS and Linux (see its documentation: it has no interrupt
//! handling and no process-group kill elsewhere). On Windows the screen is not exact (`ConPTY`
//! paints on its own timer), so latency is taken from frame events, screen reads are lenient, and
//! no decision rests on the screen. An interrupt there ends the soak where it waits, but a
//! headless scan is only ended by its bound.

mod facts;
mod headless;
mod interrupt;
mod keys;
mod limits;
mod quirks;
mod root;
mod session;
mod store_watch;
#[cfg(test)]
mod tests;
mod walk;

use std::{
    ffi::OsStr,
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

use thiserror::Error;

use crate::{
    headless::process::{Ended, ProcessError},
    metrics::milliseconds,
    pty::PtyError,
    report::{
        Document as _, HarnessSoak, SoakHeadless, SoakKind, SoakLimits, SoakOutcome, SoakRounds,
        SoakTui,
    },
    run_support::{
        FileDigest, compact_utc, format_ms, latest_can_be_replaced, point_latest_at, render_rows,
        rfc3339,
    },
    runner::{RunError, resolve_binary},
    safety::{FileIdentity, NotTheFile, ScratchError, UntrustedDirectory, check_private_directory},
    scenario::Profile,
};

pub use crate::run_support::safe_path_text;
pub use interrupt::{ARM_EVERY, Armer, Interrupt, Interrupted, Supervision};
pub use keys::{Key, Refusal};
pub use limits::{DEFAULT_RUN_LIMIT, Limits};
pub use quirks::{Quirk, QuirkLog};
pub use root::{RootError, SoakRoot, resolve_through_ancestors};

use self::{headless::HeadlessRun, session::End, session::TuiRun};

/// Opens the regular file at `path` for reading, as a person typed the path: a link among its
/// directories is followed, but its last component must not be a link, and what is opened must be
/// a regular file, which is checked on the open file and not on the path. A FIFO or a device is
/// refused without being waited on, so a path that something replaces with one cannot hold the
/// caller. It is how a scan report is read, and how the command opens the executable that cargo
/// reported for the build.
///
/// # Errors
///
/// Returns an error when the path cannot be opened, is a link, or is not a regular file.
pub fn open_regular_file(path: &Path) -> io::Result<fs::File> {
    crate::fixture::sys::open_regular_file(path)
}

/// How many rounds a soak runs when nothing else is said.
pub const DEFAULT_ROUNDS: u32 = 1;

/// The profiles of the sessions in a pseudo-terminal, in the order they run. Neither sets a key
/// preset, the mouse, or a theme: the soak runs the program as it comes.
const TUI_PROFILES: [Profile; 2] = [Profile::Default, Profile::Deterministic];
/// The profile of a headless scan.
const HEADLESS_PROFILE: Profile = Profile::Default;

/// A soak could not be run, or could not write what it found.
#[derive(Debug, Error)]
pub enum SoakError {
    /// The request asks for something that cannot be done.
    #[error("{0}")]
    Request(String),
    /// The scratch area would be inside the root, so the program would scan what the soak itself
    /// writes.
    #[error(
        "the scratch directory `{}` is inside the root, so the scan would see the soak's own \
         files; point EXCISE_E2E_TMPDIR at a directory outside the root",
        safe_path_text(.0)
    )]
    ScratchInsideRoot(PathBuf),
    /// The scratch directory, or a directory above it, can be changed by another user, or is on a
    /// file system that does not enforce ownership. The soak runs a copy of the program under test
    /// from it, once for each scan and session, by path, so that user could put another program
    /// where the copy was.
    #[error("{}", untrusted_scratch_text(.0))]
    UntrustedScratch(#[from] UntrustedDirectory),
    /// The output directory is a symbolic link, so what the soak writes would go where it points:
    /// a place nothing said when the root's path was confirmed.
    #[error(
        "the output directory `{}` is a symbolic link, so the soak's files would be written \
         where it points; move the link away, or choose another output directory",
        safe_path_text(.0)
    )]
    OutputIsALink(PathBuf),
    /// The person interrupted the run while the binary under test was being copied and hashed,
    /// before any scan started. Nothing was run, and nothing is left: the copy is gone.
    #[error(
        "the soak was interrupted while it prepared the binary under test, before any scan \
         started; nothing was run, and nothing was left behind"
    )]
    Interrupted,
    /// The bound on the whole run passed while the binary under test was being copied and hashed,
    /// before any scan started: a build or a slow disk had used it up. Nothing was run, and
    /// nothing is left: the copy is gone.
    #[error(
        "the bound on the whole run passed while the soak prepared the binary under test, before \
         any scan started; nothing was run, and nothing was left behind"
    )]
    BoundPassed,
    /// The copy of the program under test is not the file the soak made, so another program may be
    /// there. Nothing more was run.
    #[error(
        "the copy of the program under test at `{}` is not the file the soak made ({why}), so \
         nothing more was run",
        safe_path_text(.path)
    )]
    ProgramReplaced {
        /// Where the copy is.
        path: PathBuf,
        /// What is different about the file there.
        why: &'static str,
    },
    /// The binary that the caller made for the soak is not that file any more: it was replaced at
    /// its path, or written to, after it was made. Nothing was run.
    #[error(
        "the binary under test at `{}` is not the file that was made for the soak ({why}), so \
         nothing was run",
        safe_path_text(.path)
    )]
    BinaryReplaced {
        /// Where the binary is.
        path: PathBuf,
        /// What is different about the file there.
        why: &'static str,
    },
    /// The binary under test cannot be used, or a session cannot be driven.
    #[error(transparent)]
    Run(#[from] RunError),
    /// A scratch area could not be made or read.
    #[error(transparent)]
    Scratch(#[from] ScratchError),
    /// A pseudo-terminal could not be opened or the program could not be started in it.
    #[error(transparent)]
    Pty(#[from] PtyError),
    /// A headless scan could not be run.
    #[error(transparent)]
    Process(#[from] ProcessError),
    /// The person asked the soak to stop in the moment between its last look at the interrupt and
    /// the start of a scan or a session, so that program was not started ([`Interrupted`]). The
    /// run ends as an interrupted one: it is the person's request, and no harness error.
    #[error(transparent)]
    NotStarted(#[from] Interrupted),
    /// `summary.json` could not be rendered.
    #[error("cannot render summary.json: {0}")]
    Render(#[from] serde_json::Error),
    /// A file or directory could not be made or written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: &'static str,
        /// The underlying error.
        source: io::Error,
    },
}

/// What [`SoakError::UntrustedScratch`] says: the reason, why it matters, and the way out that
/// suits the reason (a mode helps with a directory that a group can write, and does nothing for a
/// volume that ignores ownership).
fn untrusted_scratch_text(refusal: &UntrustedDirectory) -> String {
    format!(
        "the scratch directory is not private to you: {refusal}. The soak runs a copy of the \
         program under test from it, so a user who can change what is in it could run another \
         program on the tree; {}",
        refusal.way_out()
    )
}

/// What a soak needs.
#[derive(Debug, Clone, Copy)]
pub struct SoakRequest<'a> {
    /// The tree to soak.
    pub root: &'a SoakRoot,
    /// The `excise` binary under test. A copy of it is what runs.
    pub binary: &'a Path,
    /// What the file at `binary` was when its maker made it, for a caller that made the file for
    /// the soak: `cargo xtask soak` copies what cargo built into its scratch area and takes the
    /// identity from the open copy once it is whole. The soak then opens `binary` without
    /// following a link and runs it only if it is that file, unchanged, both before the soak
    /// copies it and after: a file replaced at its path, or written in place, is
    /// [`SoakError::BinaryReplaced`]. `None` for a binary that nobody made for the soak (a test's
    /// script), which is copied as it is.
    pub binary_identity: Option<FileIdentity>,
    /// The directory the run's output directory is made in: `target/excise-soak` for the command.
    pub out_root: &'a Path,
    /// The existing directory the scratch areas and the copy of the binary are made in. It must not
    /// be inside the root.
    pub work_dir: &'a Path,
    /// How many rounds to run, at least 1.
    pub rounds: u32,
    /// The bounds of the run. `limits.run` is the bound on the whole run as the person set it, and
    /// the document records it as it is: what the build or the preparation of the binary used
    /// of it is not taken off.
    pub limits: Limits,
    /// The instant the bound on the whole run counts from: `cargo xtask soak` passes the instant
    /// at which it read the person's confirmation, so that the build is inside the bound, and
    /// a caller with nothing earlier to count from passes `Instant::now()`. The deadline is
    /// `started` plus `limits.run`, and it is one instant for the whole run, never a duration that
    /// is counted again from a later reading of the clock.
    pub started: Instant,
    /// Whether to keep an asciicast of each session. The screens show real names, and a recording
    /// of a scan that takes minutes is large.
    pub record: bool,
    /// The commit of the checkout the harness runs from, for the document: 40 lowercase
    /// hexadecimal digits, as `git rev-parse HEAD` prints it. Nothing else is accepted, because the
    /// document is the one file made to be shared.
    pub git_sha: &'a str,
    /// The person's interrupt.
    pub interrupt: &'a Interrupt,
}

/// What the soak shares between its parts: everything a phase needs and nothing it could change.
struct Context<'a> {
    root: &'a SoakRoot,
    binary: &'a Path,
    work_dir: &'a Path,
    limits: &'a Limits,
    run_deadline: Instant,
    interrupt: &'a Interrupt,
    record: bool,
}

/// What a soak found.
#[derive(Debug)]
pub struct SoakReport {
    /// The run's output directory, `<out_root>/<run-id>`.
    pub run_dir: PathBuf,
    /// The document that was written to `summary.json`.
    pub document: HarnessSoak,
    /// The quirks, with their text, as written to `quirks.txt`.
    pub quirks: QuirkLog,
    /// The harness error that ended the run, when one did.
    pub failure: Option<String>,
}

impl SoakReport {
    /// How the run ended.
    #[must_use]
    pub const fn outcome(&self) -> SoakOutcome {
        self.document.outcome
    }

    /// A table of what each scan and session measured: no path, no name, nothing that is not
    /// also in `summary.json`.
    #[must_use]
    pub fn table(&self) -> String {
        let mut rows: Vec<[String; 9]> = vec![
            [
                "round",
                "run",
                "profile",
                "ended",
                "scan",
                "first frame",
                "key p99",
                "peak mem",
                "open / up",
            ]
            .map(str::to_owned),
        ];
        let metric = |metrics: &std::collections::BTreeMap<String, f64>, name: &str| {
            format_ms(metrics.get(name).copied())
        };
        for scan in &self.document.headless {
            rows.push([
                scan.round.to_string(),
                "headless".to_owned(),
                scan.profile.to_string(),
                match (scan.exit_code, scan.signal) {
                    (Some(code), _) => format!("exit {code}"),
                    (None, Some(signal)) => format!("signal {signal}"),
                    (None, None) => "?".to_owned(),
                },
                format_ms(Some(scan.wall_ms)),
                "-".to_owned(),
                "-".to_owned(),
                megabytes(scan.peak_rss_bytes.map(as_f64)),
                "-".to_owned(),
            ]);
        }
        for session in &self.document.tui {
            let metrics = &session.metrics;
            rows.push([
                session.round.to_string(),
                "tui".to_owned(),
                session.profile.to_string(),
                match (session.exit.code, session.exit.signal) {
                    (Some(code), _) => format!("{} {code}", session.exit.via),
                    (None, Some(signal)) => format!("{} signal {signal}", session.exit.via),
                    (None, None) => session.exit.via.to_string(),
                },
                metric(metrics, "complete_ms"),
                metric(metrics, "first_frame_ms"),
                metric(metrics, "input_to_frame_p99_ms"),
                megabytes(metrics.get("peak_rss_bytes").copied()),
                if session.drilled {
                    format!(
                        "{} / {}",
                        metric(metrics, "drill_ms"),
                        metric(metrics, "up_ms")
                    )
                } else {
                    "-".to_owned()
                },
            ]);
        }
        render_rows(&rows)
    }
}

/// `bytes` as mebibytes, or `-`.
fn megabytes(bytes: Option<f64>) -> String {
    bytes.map_or_else(
        || "-".to_owned(),
        |bytes| format!("{:.0} MiB", bytes / (1024.0 * 1024.0)),
    )
}

#[allow(clippy::cast_precision_loss, reason = "a byte count far below 2^52")]
const fn as_f64(bytes: u64) -> f64 {
    bytes as f64
}

/// Runs a soak of `request.root`: `request.rounds` rounds of a headless scan and a session in a
/// pseudo-terminal under each profile, each bounded, the whole run bounded, and everything the run
/// found written to `<out_root>/<run-id>/` at the end. The bound on the whole run counts from
/// `request.started`: the check of the request and the preparation of the binary are inside it,
/// and so is whatever the caller did before it called.
///
/// `progress` is called with a line of text at each step: it names no path.
///
/// A run that the bound on the whole run or a person's interrupt cut short is a successful call
/// whose outcome is [`SoakOutcome::Interrupted`], and so is one that a harness error ended
/// ([`SoakOutcome::Failed`], with the error in [`SoakReport::failure`]): what finished is
/// recorded either way. A stop before any scan could start, while the binary under test is copied
/// and hashed, is the exception: nothing has finished, so the call fails, and nothing is left
/// behind.
///
/// # Errors
///
/// Returns [`SoakError`] when the request cannot be carried out (no rounds, a binary that cannot
/// be used, a scratch area inside the root), when the person's interrupt
/// ([`SoakError::Interrupted`]) or the bound on the whole run ([`SoakError::BoundPassed`]) comes
/// before the binary is ready, and when the output cannot be written.
pub fn run_soak(
    request: &SoakRequest<'_>,
    progress: &mut dyn FnMut(&str),
) -> Result<SoakReport, SoakError> {
    // A bound too long for the clock to count is refused before anything is made or started, and
    // not found out with a panic later.
    let Some(run_deadline) = request.started.checked_add(request.limits.run) else {
        return Err(SoakError::Request(
            "the bound on the whole run is too long to count".to_owned(),
        ));
    };
    check(request)?;
    let original = resolve_binary(request.binary)?;
    // What runs is a private copy of the binary, in a directory outside the root: a build in the
    // same target directory can replace the file at its path while a long soak runs, and every
    // scan and session starts the program afresh. The digest is that of the copy, so it is the
    // digest of what ran. The copy and the digest are made a chunk at a time, and what ends the
    // run, the person's interrupt or the bound, is asked before each chunk.
    let mut halted = || halt_reason(request.interrupt, run_deadline);
    let program = copy_program(
        &original,
        request.binary_identity,
        request.work_dir,
        &mut halted,
    )?;
    let digest = digest_program(&program.path, &mut halted)?;
    // The run began when its bound began to count, which is before the copy and the digest: the
    // clock that the bound runs on has no date, so it is dated by what has passed on it since.
    let started_at = wall_clock_at(request.started);
    let run_id = format!("{}-{}", compact_utc(started_at), std::process::id());
    let context = Context {
        root: request.root,
        binary: &program.path,
        work_dir: request.work_dir,
        limits: &request.limits,
        run_deadline,
        interrupt: request.interrupt,
        record: request.record,
    };

    let mut found = Found::default();
    let completed = run_rounds(&context, &program, request.rounds, progress, &mut found);

    let run_dir = create_run_dir(request.out_root, &run_id)?;
    found.save_recordings(&run_dir, &context, progress);
    let document = HarnessSoak {
        document_kind: SoakKind::HarnessSoak,
        schema_version: crate::report::SchemaVersion,
        run_id: run_id.clone(),
        started_at: rfc3339(started_at),
        finished_at: rfc3339(SystemTime::now()),
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        git_sha: request.git_sha.to_owned(),
        excise_sha256: digest,
        outcome: found.outcome(),
        rounds: SoakRounds {
            requested: request.rounds,
            completed,
        },
        limits: SoakLimits {
            run_ms: u64::try_from(request.limits.run.as_millis()).unwrap_or(u64::MAX),
        },
        headless: std::mem::take(&mut found.headless),
        tui: std::mem::take(&mut found.tui),
        quirks: found.quirks.counts(),
    };
    write_results(&run_dir, &document, &found.quirks)?;
    point_latest_at(request.out_root, &run_id).map_err(|source| SoakError::Io {
        context: "cannot point `latest` at the run",
        source,
    })?;
    Ok(SoakReport {
        run_dir,
        document,
        quirks: found.quirks,
        failure: found.failure,
    })
}

/// The wall-clock time at `started`, an instant that has already passed: the time now, less what
/// has passed since. A clock that cannot go back that far (before the epoch) is read as now.
fn wall_clock_at(started: Instant) -> SystemTime {
    let now = SystemTime::now();
    now.checked_sub(started.elapsed()).unwrap_or(now)
}

/// Refuses a request that cannot be carried out, before anything is made or started.
fn check(request: &SoakRequest<'_>) -> Result<(), SoakError> {
    if request.rounds == 0 {
        return Err(SoakError::Request(
            "a soak runs at least one round".to_owned(),
        ));
    }
    if request.limits.run.is_zero() {
        return Err(SoakError::Request(
            "the bound on the whole run must be longer than no time".to_owned(),
        ));
    }
    if !is_commit_sha(request.git_sha) {
        return Err(SoakError::Request(
            "the commit of the checkout must be 40 lowercase hexadecimal digits, as \
             `git rev-parse HEAD` prints it"
                .to_owned(),
        ));
    }
    if request.root.contains(request.work_dir) {
        return Err(SoakError::ScratchInsideRoot(request.work_dir.to_path_buf()));
    }
    check_private_directory(request.work_dir)?;
    check_output_dir(request.out_root)
}

/// Refuses an output directory that the soak could not write a run in, before anything is built or
/// started: a symbolic link (on Windows, a link or a junction), something that is not a directory,
/// or a `latest` that no run of the soak made (and so is not the soak's to replace). A directory
/// that does not exist yet is fine. `run_soak` asks this of its request, and `cargo xtask soak`
/// asks it before the prompt, so that a build is not wasted on an output directory that cannot
/// take the run.
///
/// # Errors
///
/// Returns [`SoakError`] naming what is wrong with the directory.
pub fn check_output_dir(out_root: &Path) -> Result<(), SoakError> {
    refuse_a_linked_output_dir(out_root)?;
    latest_can_be_replaced(out_root).map_err(|source| SoakError::Io {
        context: "the output directory cannot take a new run",
        source,
    })
}

/// Refuses an output directory that is a symbolic link (on Windows, a link or a junction): the
/// soak's files would be written where it points, which can be inside the tree, and nothing said
/// so when the root's path was confirmed. The directory itself is looked at, never followed, and
/// one that does not exist yet is fine. Called when the request is checked and again when the
/// directory is made, so that a link that came in between is refused too.
fn refuse_a_linked_output_dir(out_root: &Path) -> Result<(), SoakError> {
    match fs::symlink_metadata(out_root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(SoakError::OutputIsALink(out_root.to_path_buf()))
        }
        Ok(metadata) if !metadata.is_dir() => Err(SoakError::Request(format!(
            "the output directory `{}` is not a directory",
            safe_path_text(out_root)
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(SoakError::Io {
            context: "cannot inspect the output directory",
            source,
        }),
    }
}

/// Runs the rounds in turn, until they are done or the run must stop, and returns how many ran to
/// their end.
fn run_rounds(
    context: &Context<'_>,
    program: &ProgramCopy,
    rounds: u32,
    progress: &mut dyn FnMut(&str),
    found: &mut Found,
) -> u32 {
    let mut completed = 0;
    for round in 1..=rounds {
        if found.stop(context) {
            break;
        }
        // The copy is looked at again before every scan and every session: they start it by path.
        if let Err(error) = program.verify() {
            found.fail(&error);
            break;
        }
        progress(&format!("round {round}/{rounds}: headless scan"));
        match headless::run_scan(context, round, HEADLESS_PROFILE) {
            Ok(run) => {
                progress(&format!(
                    "round {round}/{rounds}: headless scan {} in {}",
                    run.finished.ended.describe(),
                    format_ms(Some(milliseconds(run.finished.wall)))
                ));
                found.take_headless(run);
            }
            // The interrupt came after the last look at it and before the scan was started: the run
            // ends as an interrupted one.
            Err(SoakError::NotStarted(_)) => {
                found.interrupted = true;
                break;
            }
            Err(error) => {
                found.fail(&error);
                break;
            }
        }
        for profile in TUI_PROFILES {
            if found.stop(context) {
                break;
            }
            if let Err(error) = program.verify() {
                found.fail(&error);
                break;
            }
            progress(&format!(
                "round {round}/{rounds}: session in a terminal ({profile})"
            ));
            match session::run_session(context, round, profile) {
                Ok(run) => {
                    progress(&format!(
                        "round {round}/{rounds}: session ({profile}) {}",
                        run.exit_summary()
                    ));
                    found.take_tui(run);
                }
                Err(SoakError::NotStarted(_)) => {
                    found.interrupted = true;
                    break;
                }
                Err(error) => {
                    found.fail(&error);
                    break;
                }
            }
        }
        // The round is complete when what was recorded says that its phases ran to their end. The
        // clock is not asked again: a bound that passed while the line of the last session was
        // written does not undo a round whose entries are all there. It is looked at before the
        // next round starts.
        if found.is_over() {
            break;
        }
        completed += 1;
    }
    completed
}

/// What the run has found so far.
#[derive(Default)]
struct Found {
    headless: Vec<SoakHeadless>,
    tui: Vec<SoakTui>,
    quirks: QuirkLog,
    /// The recordings, in temporary files outside the root, by the name they are kept under.
    recordings: Vec<(String, tempfile::TempPath)>,
    /// Whether the person or the bound on the whole run ended the run.
    interrupted: bool,
    /// The harness error that ended the run.
    failure: Option<String>,
}

impl Found {
    /// How the run ended: a harness error outranks an interruption.
    const fn outcome(&self) -> SoakOutcome {
        if self.failure.is_some() {
            SoakOutcome::Failed
        } else if self.interrupted {
            SoakOutcome::Interrupted
        } else {
            SoakOutcome::Finished
        }
    }

    /// Whether what was recorded ends the run: a phase that was cut short, or a harness error. It
    /// reads the record and never the clock, so that a round whose phases all ran to their end is
    /// not undone by a bound that passed after the last of them.
    const fn is_over(&self) -> bool {
        self.failure.is_some() || self.interrupted
    }

    /// Whether the run must stop before it starts anything else: what was recorded ends it, the
    /// person interrupted it, or the bound on the whole run has passed.
    fn stop(&mut self, context: &Context<'_>) -> bool {
        if self.is_over() {
            return true;
        }
        if context.interrupt.is_set() || Instant::now() >= context.run_deadline {
            self.interrupted = true;
        }
        self.interrupted
    }

    fn take_headless(&mut self, run: HeadlessRun) {
        self.interrupted |= run.end != End::Session;
        self.headless.push(headless_entry(&run));
        self.quirks.append(run.quirks);
    }

    fn take_tui(&mut self, run: TuiRun) {
        self.interrupted |= run.end != End::Session;
        let name = format!("{}-tui-{}.cast", run.round, run.profile);
        let entry = SoakTui {
            round: run.round,
            profile: run.profile,
            exit: run.exit,
            timed_out_phase: run.timed_out_phase,
            terminal_restored: run.terminal_restored,
            residue_files: run.residue.len() as u64,
            drilled: run.drilled,
            metrics: run.metrics,
        };
        self.tui.push(entry);
        self.quirks.append(run.quirks);
        if let Some(recording) = run.recording {
            self.recordings.push((name, recording));
        }
        if let Some(error) = run.error {
            self.failure = Some(error.to_string());
        }
    }

    fn fail(&mut self, error: &SoakError) {
        self.quirks.note(
            crate::report::QuirkKind::HarnessError,
            "the run",
            error.to_string(),
        );
        self.failure = Some(error.to_string());
    }

    /// Copies the recordings into `run_dir`, one at a time and in chunks, and stops at the
    /// person's interrupt or the end of the run's clock (see [`SAVE_GRACE`]): a run whose copy was
    /// cut short is `interrupted`, and the recordings after the cut are not saved. A copy that
    /// fails is a harness error, and ends the saving.
    fn save_recordings(
        &mut self,
        run_dir: &Path,
        context: &Context<'_>,
        progress: &mut dyn FnMut(&str),
    ) {
        if self.recordings.is_empty() {
            return;
        }
        progress(&format!("saving {} recording(s)", self.recordings.len()));
        let deadline = context
            .run_deadline
            .checked_add(SAVE_GRACE)
            .unwrap_or(context.run_deadline);
        let mut stop = || saving_must_stop(context.interrupt, deadline);
        let recordings = std::mem::take(&mut self.recordings);
        for (name, temporary) in &recordings {
            let copied = fs::File::open(temporary).and_then(|mut source| {
                copy_bounded(
                    &mut source,
                    COPY_CHUNK,
                    || private_file(&run_dir.join(name)),
                    &mut stop,
                )
            });
            match copied {
                Ok(Copied::Whole(_)) => {}
                Ok(Copied::Cut(_)) => {
                    self.interrupted = true;
                    break;
                }
                Err(source) => {
                    self.fail(&SoakError::Io {
                        context: "cannot keep a recording",
                        source,
                    });
                    break;
                }
            }
        }
    }
}

impl TuiRun {
    /// How the session ended, in a few words, for the progress line.
    fn exit_summary(&self) -> String {
        match (self.exit.code, self.exit.signal) {
            (Some(code), _) => format!("{} with exit code {code}", self.exit.via),
            (None, Some(signal)) => format!("{} by signal {signal}", self.exit.via),
            (None, None) => self.exit.via.to_string(),
        }
    }
}

/// The document's entry for a headless scan.
fn headless_entry(run: &HeadlessRun) -> SoakHeadless {
    let finished = &run.finished;
    SoakHeadless {
        round: run.round,
        profile: run.profile,
        exit_code: match finished.ended {
            Ended::Exited(code) => Some(i64::from(code)),
            Ended::Signaled(_) => None,
        },
        signal: match finished.ended {
            Ended::Signaled(signal) => Some(i64::from(signal)),
            Ended::Exited(_) => None,
        },
        timed_out: finished.timed_out,
        wall_ms: milliseconds(finished.wall),
        user_ms: finished.cpu.map(|cpu| milliseconds(cpu.user)),
        sys_ms: finished.cpu.map(|cpu| milliseconds(cpu.system)),
        peak_rss_bytes: finished.peak_memory_bytes,
        residue_files: run.residue.len() as u64,
        report: run.facts.as_ref().map(|read| read.facts),
    }
}

/// How many bytes of a recording, or of the binary under test, are copied between two looks at the
/// interrupt and the clock.
const COPY_CHUNK: usize = 1 << 20;

/// How long after the end of the run's bound the recordings may still be saved. The bound ends the
/// sessions, what they recorded is what a person asked to keep, and a copy is quick, so saving
/// gets this much more. A person's interrupt gets none: it ends the copy where it is.
const SAVE_GRACE: Duration = Duration::from_secs(30);

/// Whether saving the recordings must stop now: the person interrupted the run, or the clock of
/// the run, with the grace that only saving may use, has run out.
fn saving_must_stop(interrupt: &Interrupt, deadline: Instant) -> bool {
    interrupt.is_set() || Instant::now() >= deadline
}

/// How the copy of a recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Copied {
    /// Every byte was copied, this many.
    Whole(u64),
    /// `stop` said so between two chunks, after this many bytes, with more left to copy.
    Cut(u64),
}

/// Copies `source` to the file that `open` makes, `chunk` bytes at a time, and asks `stop` before
/// each chunk, the first included. The file is made when its first chunk is about to be written, so
/// a copy that is stopped at once leaves nothing; a stop that comes after the last byte was copied
/// is not a cut.
fn copy_bounded<R: Read, W: Write>(
    source: &mut R,
    chunk: usize,
    open: impl FnOnce() -> io::Result<W>,
    stop: &mut impl FnMut() -> bool,
) -> io::Result<Copied> {
    let mut buffer = vec![0_u8; chunk];
    let mut open = Some(open);
    let mut target: Option<W> = None;
    let mut copied = 0_u64;
    loop {
        if stop() {
            return Ok(if source.read(&mut buffer)? == 0 {
                Copied::Whole(copied)
            } else {
                Copied::Cut(copied)
            });
        }
        let read = source.read(&mut buffer)?;
        if read == 0 {
            if let Some(open) = open.take() {
                open()?;
            }
            return Ok(Copied::Whole(copied));
        }
        if target.is_none() {
            let make = open
                .take()
                .ok_or_else(|| io::Error::other("the file is made once"))?;
            target = Some(make()?);
        }
        if let Some(writer) = target.as_mut() {
            writer.write_all(&buffer[..read])?;
            copied += read as u64;
        }
    }
}

/// Makes the run's directory below `out_root`, private to its owner on Unix: the quirk log and the
/// recordings can name things in the tree.
///
/// The output directory is made one level at a time, with a call that never follows a link at its
/// end, and looked at afterwards: a link there (one that came in since the request was checked)
/// is refused, and nothing is written through it.
fn create_run_dir(out_root: &Path, run_id: &str) -> Result<PathBuf, SoakError> {
    let io_error = |context: &'static str| move |source| SoakError::Io { context, source };
    if let Some(parent) = out_root.parent() {
        fs::create_dir_all(parent).map_err(io_error("cannot create the output directory"))?;
    }
    match fs::create_dir(out_root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(source) => {
            return Err(SoakError::Io {
                context: "cannot create the output directory",
                source,
            });
        }
    }
    refuse_a_linked_output_dir(out_root)?;
    let run_dir = out_root.join(run_id);
    create_private_dir(&run_dir).map_err(io_error("cannot create the run's directory"))?;
    Ok(run_dir)
}

/// Writes `quirks.txt` and then `summary.json`, the last file of a run: the document says how the
/// run ended once everything else, the recordings included, was done. `summary.json` is published
/// whole or not at all ([`publish`]), so that a run directory that has one has a whole one.
fn write_results(
    run_dir: &Path,
    document: &HarnessSoak,
    quirks: &QuirkLog,
) -> Result<(), SoakError> {
    let io_error = |context: &'static str| move |source| SoakError::Io { context, source };
    write_private(
        &run_dir.join("quirks.txt"),
        quirks.render(&document.run_id).as_bytes(),
    )
    .map_err(io_error("cannot write quirks.txt"))?;
    let summary = document.to_json_pretty()?;
    let published = publish(run_dir, "summary.json", |file| {
        file.write_all(summary.as_bytes())
    });
    published.map_err(io_error("cannot write summary.json"))
}

/// Makes `<dir>/<name>` hold what `produce` writes, whole or not at all. `produce` writes into a
/// private temporary file made in `dir`, which is flushed to the disk and only then takes the name
/// `name`, and never in place of a file that is there. Anything that fails on the way, a full disk
/// or an exhausted quota included, removes the temporary file and leaves `name` unmade, so that a
/// `name` that exists is a whole file.
fn publish(
    dir: &Path,
    name: &str,
    produce: impl FnOnce(&mut dyn Write) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = tempfile::NamedTempFile::new_in(dir)?;
    produce(file.as_file_mut())?;
    file.as_file().sync_all()?;
    file.persist_noclobber(dir.join(name))?;
    Ok(())
}

fn create_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        fs::DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        fs::DirBuilder::new().create(path)
    }
}

/// A new file that only its owner can use on Unix, with `mode`. It must not exist, and a link at
/// its name is not followed.
fn new_private_file(path: &Path, mode: u32) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;

        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    options.open(path)
}

fn private_file(path: &Path) -> io::Result<fs::File> {
    new_private_file(path, 0o600)
}

/// The mode `0700`: what keeps a directory or a file private to its owner. Unix only.
#[cfg(unix)]
fn owner_only() -> fs::Permissions {
    use std::os::unix::fs::PermissionsExt as _;

    fs::Permissions::from_mode(0o700)
}

/// Gives the directory at `path`, which was just made by the soak, the mode `0700` exactly: the
/// mode it was made with went through the umask. Unix only, where a mode is what keeps the
/// directory private to its owner.
#[cfg(unix)]
fn make_directory_owner_only(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, owner_only())
}

/// Gives `file`, which was just made by the soak, the mode `0700` exactly: the mode it was made
/// with went through the umask. The mode is set on the open file, so it can reach no other. Unix
/// only.
#[cfg(unix)]
fn make_file_owner_only(file: &fs::File) -> io::Result<()> {
    file.set_permissions(owner_only())
}

/// The program a soak runs: a copy of the binary under test, in a directory of its own.
struct ProgramCopy {
    /// Holds the directory, and with it the copy, until the run is over.
    _directory: tempfile::TempDir,
    path: PathBuf,
    /// What the file that was made is.
    identity: FileIdentity,
}

impl ProgramCopy {
    /// Looks at the copy again: the file at its path must still be the one that was made, and
    /// nobody but its owner can write it. Asked before each scan and each session, which start it
    /// by that path. The copy is a second line behind the refusal of a scratch directory that
    /// other users can change ([`check_private_directory`]): a directory that was renamed away and
    /// made again, a file that was replaced, or a link that was put there is not the file that was
    /// copied, and what is there is not run.
    fn verify(&self) -> Result<(), SoakError> {
        self.identity
            .check(&self.path)
            .map_err(|changed| SoakError::ProgramReplaced {
                path: self.path.clone(),
                why: changed.why(),
            })
    }
}

/// Opens the binary under test for copying. When its maker handed the soak what it made (`made`),
/// the file is opened without following a link at its end and without waiting, and checked to be
/// a regular file (`open_regular_file`), and it must be that file, unchanged: what is at the path
/// can have been replaced since it was made. The question is asked of the open file, so that no
/// path can be swapped between the question and the copy.
fn open_binary(binary: &Path, made: Option<FileIdentity>) -> Result<fs::File, SoakError> {
    let cannot_read = |source| SoakError::Io {
        context: "cannot read the binary",
        source,
    };
    let Some(made) = made else {
        return fs::File::open(binary).map_err(cannot_read);
    };
    let file = open_regular_file(binary).map_err(cannot_read)?;
    made.check_open(&file)
        .map_err(|changed| replaced(binary, changed))?;
    Ok(file)
}

/// The refusal for a binary that is not the file that was made for the soak.
fn replaced(binary: &Path, changed: NotTheFile) -> SoakError {
    SoakError::BinaryReplaced {
        path: binary.to_path_buf(),
        why: changed.why(),
    }
}

/// A new directory in `work_dir` for the copy of the binary, made with the mode `0700` asked for,
/// so that it is private to its owner from the moment it exists: the mode that a directory is
/// made with by default is what the umask leaves, `0755` under the usual one and more under a
/// permissive one, and another user who watched the scratch directory could put an entry in it,
/// under the name of the copy, before the mode was changed. (A umask also filters this mode: the
/// caller gives the directory `0700` exactly once it is made.)
fn directory_for_the_copy(work_dir: &Path) -> io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    builder.prefix("xh-soak-bin-");
    #[cfg(unix)]
    builder.permissions(owner_only());
    builder.tempdir_in(work_dir)
}

/// Copies `binary` into a new directory in `work_dir`, which is outside the root and private to its
/// owner, and says where the copy is. The copy is made from a file that must not exist, and it is
/// closed before anything runs it: a program that is open for writing cannot be started.
///
/// When `made` says what the file at `binary` was when its maker made it, the file that is opened
/// must be that file ([`open_binary`]), and it must still be after it was copied: nothing wrote to
/// it while it was read, which would have left a copy of a mixture, and the copy is of what was
/// made.
///
/// The directory is made private from the moment it exists ([`directory_for_the_copy`]), and the
/// directory and the copy are then given the mode `0700` exactly: the mode that a directory or a
/// file is made with is filtered through the umask of the process, and a umask that clears one of
/// an owner's bits would leave the directory that cannot be entered, or the copy that cannot be
/// read or cannot be run.
///
/// What identifies the copy is read from the copy once it is whole, after the last write and the
/// last change of mode, from a second handle to the open file and not from its path.
///
/// It is made a chunk at a time, and `halted` is asked before each chunk, the first included: what
/// it returns ends the copy at once, as the error of the call, and the directory, with whatever
/// was copied into it, is removed.
fn copy_program(
    binary: &Path,
    made: Option<FileIdentity>,
    work_dir: &Path,
    halted: &mut impl FnMut() -> Option<SoakError>,
) -> Result<ProgramCopy, SoakError> {
    let io_error = |context: &'static str| move |source| SoakError::Io { context, source };
    let directory = directory_for_the_copy(work_dir).map_err(io_error(
        "cannot make a directory for the copy of the binary",
    ))?;
    #[cfg(unix)]
    make_directory_owner_only(directory.path()).map_err(io_error(
        "cannot make the directory for the copy of the binary private to its owner",
    ))?;
    let path = directory
        .path()
        .join(binary.file_name().unwrap_or_else(|| OsStr::new("excise")));
    let mut original = open_binary(binary, made)?;
    let mut stopped = None;
    let made_copy = std::cell::RefCell::new(None::<fs::File>);
    copy_bounded(
        &mut original,
        COPY_CHUNK,
        || {
            let copy = new_private_file(&path, 0o700)?;
            #[cfg(unix)]
            make_file_owner_only(&copy)?;
            *made_copy.borrow_mut() = Some(copy.try_clone()?);
            Ok(copy)
        },
        &mut || {
            stopped = halted();
            stopped.is_some()
        },
    )
    .map_err(io_error("cannot copy the binary"))?;
    if let Some(reason) = stopped {
        return Err(reason);
    }
    if let Some(made) = made {
        made.check_open(&original)
            .map_err(|changed| replaced(binary, changed))?;
    }
    let copy = made_copy.into_inner().ok_or_else(|| SoakError::Io {
        context: "cannot copy the binary",
        source: io::Error::other("no file was made"),
    })?;
    // What identifies the file is read from the file that was made, whole, and not from its path.
    let identity = FileIdentity::of(&copy).map_err(io_error(
        "cannot take the identity of the copy of the binary",
    ))?;
    // Closed before anything runs it: a program that is open for writing cannot be started.
    drop(copy);
    Ok(ProgramCopy {
        _directory: directory,
        path,
        identity,
    })
}

/// The SHA-256 of the copy of the binary, in lowercase hexadecimal: the digest of what runs. It is
/// taken a chunk at a time, and `halted` is asked before each chunk, the first included: what it
/// returns ends the digest at once, as the error of the call.
fn digest_program(
    copy: &Path,
    halted: &mut impl FnMut() -> Option<SoakError>,
) -> Result<String, SoakError> {
    let io_error = |source| SoakError::Io {
        context: "cannot read the binary to take its digest",
        source,
    };
    let mut digest = FileDigest::open(copy).map_err(io_error)?;
    loop {
        if let Some(reason) = halted() {
            return Err(reason);
        }
        if !digest.step().map_err(io_error)? {
            return Ok(digest.finish());
        }
    }
}

/// Why the run must stop now, if it must: the person interrupted it, which is said first when both
/// are so, or the bound on the whole run has passed.
fn halt_reason(interrupt: &Interrupt, deadline: Instant) -> Option<SoakError> {
    if interrupt.is_set() {
        Some(SoakError::Interrupted)
    } else if Instant::now() >= deadline {
        Some(SoakError::BoundPassed)
    } else {
        None
    }
}

/// Whether `text` is a commit as `git rev-parse HEAD` prints one: 40 lowercase hexadecimal digits.
/// It goes into `summary.json`, the one file made to be shared, so it is never free text.
fn is_commit_sha(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    private_file(path)?.write_all(bytes)
}
