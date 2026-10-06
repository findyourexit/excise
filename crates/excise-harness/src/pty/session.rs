//! A program running in a pseudo-terminal of its own process group.
//!
//! [`PtySession`] owns everything about one run: the pseudo-terminal, the child, a reader thread
//! that forwards terminal output, the screen model that output feeds, the recording, and the
//! resource samples. It is a single-owner state machine: the caller drives it by calling
//! [`PtySession::pump`] (or [`PtySession::wait_activity`]) between checks, and nothing changes the
//! screen or the exit state behind its back.
//!
//! # Process ownership
//!
//! The pseudo-terminal library starts the child as the leader of a new session, so the child is
//! also the leader of its own process group. [`PtySession::kill`] signals that group with
//! `SIGKILL`, so nothing the child forked survives, and the same happens when the session is
//! dropped: a session never leaks a process. The child is reaped only after its exit is seen
//! without reaping (`waitid` with `WNOWAIT`), so its final resource figures can still be read,
//! and so that what it started and left running can be killed while its process id still names
//! its group: a leader that has ended is a zombie until it is reaped, and nothing else can have
//! the id. The kill is made at that moment, once, so a child that ends and leaves a descendant
//! behind does not leave it running.
//!
//! The library's writer sends a newline and an end-of-file character to the child when it is
//! dropped. That input could confirm a dialog, so the session drops the writer only once the child
//! is dead.
//!
//! # Windows
//!
//! The child is a `ConPTY` process. It is ended with `TerminateProcess`, exits are found by polling,
//! and the terminal's echo and canonical modes are unavailable. See `safety::process`.
//!
//! `ConPTY` also keeps its own copy of the screen and sends what changed on a timer, so output can
//! reach the reader tens of milliseconds after the program wrote it. A step that waits for a frame
//! event allows for that (`runner::live::CONPTY_FRAME_WINDOW`).

use std::{
    ffi::OsString,
    fmt,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use thiserror::Error;

use crate::{
    fixture::path::write_escaped,
    metrics::{CpuTimes, ProcessSampler, reaped_children_cpu},
};

use super::{
    cast::{CastHeader, CastWriter},
    marks::{MarkScanner, Piece},
    screen::{Screen, ScreenModes},
};

/// How many marks a session remembers. The waits that ask about a mark ask about a recent one, and
/// a mark that has left the log is answered by the oldest one kept, which was read after it.
const MARKS_KEPT: usize = 4096;

/// A mark the session has read, and what came after it.
#[derive(Debug, Clone, Copy)]
struct MarkRead {
    /// The number of the frame the mark follows.
    seq: u64,
    /// When the mark was read.
    at: Instant,
    /// When the first output that is not a mark was read after the mark: the paint of a console
    /// host that paints on its own timer. `None` until some has come.
    first_paint_at: Option<Instant>,
}

/// What a session keeps of the screen as the latest mark left it ([`PtySession::marked_screen`]).
enum Marked {
    /// The session was not asked to keep it: the screen model is all there is.
    NotKept,
    /// It was asked, and there is no screen of a mark to keep yet: no mark has been read, or the
    /// terminal was resized since the copy was made.
    NoMarkYet,
    /// The latest mark is the last thing read. The screen model is exactly the screen it left,
    /// and the next output is the start of the redraw after it.
    AtTheMark,
    /// Output has been read after the latest mark. This is the screen as the mark left it, copied
    /// when that output first came.
    Kept(Box<Screen>),
}

/// How often a running child is sampled for memory, threads, and descriptors.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
/// How long [`PtySession::kill`] waits for a killed child to die before it gives up.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long dropping a session waits for the reader thread to finish.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// The size of one read from the terminal.
const READ_CHUNK: usize = 64 * 1024;
/// How many bytes of output one call of [`PtySession::pump`] takes in at most. A program that
/// writes faster than the session reads would keep a call going for as long as it writes, and the
/// caller, whose deadlines are only looked at between calls, would never get its turn. What is
/// left waits for the next call.
const PUMP_LIMIT: usize = 1024 * 1024;
/// How many bytes of output wait to be pumped, at most, not counting the read that the reader
/// thread has in hand (one [`READ_CHUNK`]). The thread stops reading when this much waits, so a
/// program that writes faster than the session pumps is held back in its own writes, as it is by
/// a terminal that nobody reads. Without the bound what waits would grow as long as the program
/// writes, and a caller that is not pumping, or is pumping one [`PUMP_LIMIT`] at a time, could not
/// stop it. It is a number of bytes, and not of reads, because how much one read returns is the
/// system's to decide. [`PtySession::kill`] lifts it: a program that is being killed must be read
/// freely, because it does not die while a write of its own is held back.
const QUEUE_BYTES: usize = 4 * 1024 * 1024;
/// How often a reader thread that waits for room in the queue looks again.
const BACKLOG_POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How many bytes of raw output a timeout's diagnostics keep from the start and from the end of
/// the stream.
const DIAGNOSTIC_BYTES: usize = 200;
/// How often a capped reader thread rechecks its token bucket while no bytes are banked yet, so
/// it notices a lifted cap (`PtySession::kill`) promptly instead of sleeping through it.
const DRAIN_CAP_POLL_INTERVAL: Duration = Duration::from_millis(5);

/// A session could not be started or driven.
#[derive(Debug, Error)]
pub enum PtyError {
    /// The program is not an absolute path. A cleared environment has no `PATH` to search.
    #[error(
        "the program `{}` is not an absolute path; the isolated environment has no PATH to \
         search",
        .0.display()
    )]
    RelativeProgram(PathBuf),
    /// The pseudo-terminal could not be opened.
    #[error("cannot open a pseudo-terminal: {0}")]
    Open(String),
    /// The program could not be started.
    #[error("cannot start `{}` in the pseudo-terminal: {reason}", program.display())]
    Spawn {
        /// The program.
        program: PathBuf,
        /// Why not.
        reason: String,
    },
    /// The recording could not be created or written.
    #[error("cannot record the session to `{}`: {source}", path.display())]
    Recording {
        /// The recording file.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },
    /// Input could not be written to the terminal.
    #[error("cannot write to the terminal: {0}")]
    Input(io::Error),
    /// The terminal input was closed.
    #[error("the terminal input is closed")]
    InputClosed,
    /// The terminal's output could not be read. Not the end of the output: that is a read that
    /// returns nothing, or fails because the program's side of the terminal is closed.
    #[error("cannot read from the terminal: {0}")]
    Read(io::Error),
    /// The terminal could not be resized.
    #[error("cannot resize the terminal: {0}")]
    Resize(String),
}

/// What to run and how.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    /// The program. It must be an absolute path.
    pub program: PathBuf,
    /// Its arguments.
    pub args: Vec<OsString>,
    /// Its complete environment. Nothing is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// Its working directory.
    pub cwd: PathBuf,
    /// The terminal width in columns.
    pub cols: u16,
    /// The terminal height in rows.
    pub rows: u16,
    /// Caps how fast the reader thread drains the terminal's output, in bytes per second, so a
    /// scenario can simulate a slow terminal and the backpressure it puts on the child's writes.
    /// `None` (unchanged behavior) drains as fast as the operating system delivers bytes. The cap
    /// stops applying, and the reader drains freely to the end of the output, the instant
    /// [`PtySession::kill`] is called: see its documentation for why.
    pub drain_bytes_per_sec: Option<u64>,
    /// Where to write the asciicast recording, if anywhere.
    pub recording: Option<PathBuf>,
    /// A title for the recording.
    pub title: Option<String>,
}

/// How a child ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitInfo {
    /// The exit code, if the child exited by itself.
    pub code: Option<i32>,
    /// The signal that killed it, if it was killed. Always `None` on Windows.
    pub signal: Option<i32>,
    /// When the session first saw the exit.
    pub at: Instant,
}

impl ExitInfo {
    /// A short description: `exit code 0` or `killed by signal 15`.
    #[must_use]
    pub fn describe(&self) -> String {
        match (self.code, self.signal) {
            (_, Some(signal)) => format!("killed by signal {signal}"),
            (Some(code), None) => format!("exit code {code}"),
            (None, None) => "an unknown status".to_owned(),
        }
    }
}

/// The modes of the terminal: the display modes the emulator tracks and the line-discipline modes
/// of the pseudo-terminal itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalModes {
    /// The alternate screen is active.
    pub alternate_screen: bool,
    /// The cursor is visible.
    pub cursor_visible: bool,
    /// The terminal echoes input. `None` where the platform cannot say (Windows).
    pub echo: Option<bool>,
    /// The terminal is in canonical (line-buffered) mode. `None` where the platform cannot say.
    pub icanon: Option<bool>,
}

impl TerminalModes {
    /// Whether the terminal looks as a shell left it: the alternate screen left, the cursor
    /// visible, and echo and canonical mode on. A mode the platform cannot report counts as
    /// restored.
    #[must_use]
    pub fn restored(&self) -> bool {
        !self.alternate_screen
            && self.cursor_visible
            && self.echo.unwrap_or(true)
            && self.icanon.unwrap_or(true)
    }
}

/// One read from the terminal.
struct Chunk {
    at: Instant,
    bytes: Vec<u8>,
}

/// Everything a session can say about its output and its child, for a step that timed out: how
/// much output arrived and when, whether the child is still alive, how many cursor position
/// report requests the screen model has answered, and the bounded ends of the raw stream (see
/// [`DIAGNOSTIC_BYTES`]). A step waits on the screen model, never on raw bytes (see
/// [`crate::pty::Screen`]), so this is evidence for a human (and, via
/// `report::SessionDiagnostics`, a failure document) reading a failure, not something a scenario
/// can assert on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostics {
    /// Every byte of output read so far.
    pub output_bytes: u64,
    /// From the spawn to the first byte of output, once any has arrived.
    pub first_byte_after: Option<Duration>,
    /// How the child last reported, once the session has seen it end.
    pub exit: Option<ExitInfo>,
    /// How many `ESC [ 6 n` cursor position report requests the screen model has answered.
    pub cursor_reports_answered: u32,
    /// The first bytes of output.
    pub head: Vec<u8>,
    /// The last bytes of output.
    pub tail: Vec<u8>,
}

impl fmt::Display for Diagnostics {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} output bytes received", self.output_bytes)?;
        if let Some(after) = self.first_byte_after {
            write!(formatter, " (the first after {} ms)", after.as_millis())?;
        }
        write!(
            formatter,
            "; the program is {}",
            self.exit.map_or_else(
                || "still running".to_owned(),
                |exit| format!("not running ({})", exit.describe())
            )
        )?;
        write!(
            formatter,
            "; {} cursor-position requests answered",
            self.cursor_reports_answered
        )?;
        write!(formatter, "; first bytes \"")?;
        write_escaped(formatter, &self.head)?;
        write!(formatter, "\", last bytes \"")?;
        write_escaped(formatter, &self.tail)?;
        write!(formatter, "\"")
    }
}

/// A running (or finished) program in a pseudo-terminal.
pub struct PtySession {
    child: Box<dyn Child + Send + Sync>,
    pid: u32,
    /// Whether the spawn was wrapped under the Linux cgroup memory cap. `read_memory_peak` is
    /// then tried through `pid` at the same "zombie, not yet reaped" moment the resource sampler
    /// already uses; see `safety::cgroup`.
    cgroup_wrapped: bool,
    /// The cgroup's `memory.peak`, once read. `None` until the child has exited, and also when
    /// `cgroup_wrapped` is false or the counter could not be read.
    cgroup_peak_bytes: Option<u64>,
    /// `None` once [`PtySession::close_console`] has closed it.
    master: Option<Box<dyn MasterPty + Send>>,
    /// Dropped only after the child is dead; see the module documentation.
    writer: Option<Box<dyn Write + Send>>,
    chunks: Receiver<io::Result<Chunk>>,
    /// How much output waits in `chunks`, and whether anybody is going to take it in.
    backlog: Arc<Backlog>,
    reader: Option<JoinHandle<()>>,
    reader_done: bool,
    /// Lifted by [`PtySession::kill`] so a capped reader drains freely once the run ends.
    uncapped: Arc<AtomicBool>,
    screen: Screen,
    /// Takes the frame marks out of the output before the screen model sees it.
    marks: MarkScanner,
    /// The latest frame whose mark the session has read. Zero until the first mark.
    frame_shown: u64,
    /// How many of the newest marks in `marks_read` no output that is not a mark has followed yet.
    unpainted: usize,
    /// The latest marks read, oldest first, and what came after each (see
    /// [`PtySession::paint_followed_mark`]). At most [`MARKS_KEPT`].
    marks_read: std::collections::VecDeque<MarkRead>,
    /// What the session keeps of the screen as the latest mark left it
    /// ([`PtySession::marked_screen`]).
    marked: Marked,
    recording: Option<(PathBuf, CastWriter<io::BufWriter<std::fs::File>>)>,
    started: Instant,
    exit: Option<ExitInfo>,
    reaped: bool,
    output_bytes: u64,
    /// When the first byte of output arrived, for a timeout's diagnostics.
    first_byte_at: Option<Instant>,
    /// The first bytes of output, for a timeout's diagnostics. See [`DIAGNOSTIC_BYTES`].
    head: Vec<u8>,
    /// The last bytes of output, for a timeout's diagnostics. See [`DIAGNOSTIC_BYTES`].
    tail: Vec<u8>,
    sampler: ProcessSampler,
    last_sample: Instant,
    cpu_before: Option<CpuTimes>,
    cpu_after: Option<CpuTimes>,
}

impl std::fmt::Debug for PtySession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PtySession")
            .field("pid", &self.pid)
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

impl PtySession {
    /// Starts `spec.program` in a new pseudo-terminal.
    ///
    /// # Errors
    ///
    /// Returns an error if the program path is relative, the pseudo-terminal cannot be opened, the
    /// recording cannot be created, or the program cannot be started.
    pub fn spawn(spec: &SpawnSpec) -> Result<Self, PtyError> {
        if !spec.program.is_absolute() {
            return Err(PtyError::RelativeProgram(spec.program.clone()));
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: spec.rows,
                cols: spec.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| PtyError::Open(format!("{error:#}")))?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| PtyError::Open(format!("{error:#}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| PtyError::Open(format!("{error:#}")))?;

        // Every fallible step that does not need the child comes first, so a failure leaves
        // nothing running.
        let (sender, chunks) = mpsc::channel();
        let uncapped = Arc::new(AtomicBool::new(false));
        let backlog = Arc::new(Backlog::default());
        let reader = spawn_reader(
            reader,
            sender,
            spec.drain_bytes_per_sec,
            Arc::clone(&uncapped),
            Arc::clone(&backlog),
        )?;
        let started = Instant::now();
        let recording = spec
            .recording
            .as_ref()
            .map(|path| open_recording(path, spec, started).map(|cast| (path.clone(), cast)))
            .transpose()?;

        let mut command = CommandBuilder::new(&spec.program);
        command.env_clear();
        for argument in &spec.args {
            command.arg(argument);
        }
        command.cwd(&spec.cwd);
        for (name, value) in &spec.env {
            command.env(name, value);
        }
        let cpu_before = reaped_children_cpu();
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| PtyError::Spawn {
                program: spec.program.clone(),
                reason: format!("{error:#}"),
            })?;
        // The child holds the slave now. Keeping ours open would stop the reader from ever seeing
        // the end of the output.
        drop(pair.slave);
        let pid = child.process_id().ok_or_else(|| PtyError::Spawn {
            program: spec.program.clone(),
            reason: "the child has no process id".to_owned(),
        })?;

        Ok(Self {
            child,
            pid,
            cgroup_wrapped: false,
            cgroup_peak_bytes: None,
            master: Some(pair.master),
            writer: Some(writer),
            chunks,
            backlog,
            reader: Some(reader),
            reader_done: false,
            uncapped,
            screen: Screen::new(spec.rows, spec.cols),
            marks: MarkScanner::default(),
            frame_shown: 0,
            unpainted: 0,
            marks_read: std::collections::VecDeque::new(),
            marked: Marked::NotKept,
            recording,
            started,
            exit: None,
            reaped: false,
            output_bytes: 0,
            first_byte_at: None,
            head: Vec::new(),
            tail: Vec::new(),
            sampler: ProcessSampler::default(),
            last_sample: started.checked_sub(SAMPLE_INTERVAL).unwrap_or(started),
            cpu_before,
            cpu_after: None,
        })
    }

    /// The process id of the child, which is also its process group id on Unix.
    #[must_use]
    pub const fn pid(&self) -> u32 {
        self.pid
    }

    /// Tells the session that its direct child is `systemd-run`, wrapping the real program under
    /// the Linux cgroup memory cap (`safety::cgroup`): `kill`, the process-group id, and reaping
    /// are unaffected (they already operate on the whole process group), but `poll_exit` also
    /// tries to read the cgroup's `memory.peak` once the child has exited. Called by `runner::run`
    /// right after a wrapped spawn, before the first step runs.
    pub(crate) fn mark_cgroup_wrapped(&mut self) {
        self.cgroup_wrapped = true;
    }

    /// The cgroup's `memory.peak`, when the spawn was wrapped under the Linux cgroup memory cap
    /// and the child has exited: see [`PtySession::mark_cgroup_wrapped`]. `None` before the child
    /// exits, when the session was not wrapped, or when the counter could not be read.
    #[must_use]
    pub const fn cgroup_memory_peak_bytes(&self) -> Option<u64> {
        self.cgroup_peak_bytes
    }

    /// When the child was spawned.
    #[must_use]
    pub const fn started(&self) -> Instant {
        self.started
    }

    /// The screen model.
    #[must_use]
    pub const fn screen(&self) -> &Screen {
        &self.screen
    }

    /// The latest frame whose mark the session has read, or zero when it has read none. On a Unix
    /// pseudo-terminal the mark follows its frame's bytes, so the screen model has been through
    /// every byte of that frame and of every frame before it. A console host that paints on its own
    /// timer (`ConPTY`) delivers the mark before the paint of its frame: there the mark says that
    /// the host has the frame, and [`PtySession::paint_followed_mark`] says when the screen shows
    /// it. A program that does not mark its frames never moves it (see the `pty` module); one that
    /// does moves it once for every frame it draws.
    #[must_use]
    pub const fn frame_shown(&self) -> u64 {
        self.frame_shown
    }

    /// Makes the session keep the screen as the latest mark left it
    /// ([`PtySession::marked_screen`]). It is not the default: a copy of the screen is made each
    /// time output is read after a mark, and a runner that never asks for the screen of a frame
    /// does not pay for it. Called before any output is read.
    pub fn keep_the_screen_of_each_mark(&mut self) {
        if matches!(self.marked, Marked::NotKept) {
            self.marked = Marked::NoMarkYet;
        }
    }

    /// The screen as the latest mark left it, for a read that is about one frame and not about a
    /// redraw in progress.
    ///
    /// The screen model is fed as the output arrives, so after a read it can hold the start of the
    /// redraw of the frame after the latest mark (a read ends where it ends, in the middle of a
    /// frame as often as not): a header whose badge is half drawn, a pane that shows the old
    /// entry above the new one's size. On a Unix pseudo-terminal the mark follows its frame's
    /// bytes, so the screen as the mark left it is the screen of [`PtySession::frame_shown`]
    /// exactly. That is this: a copy made when output was first read after the mark, and the
    /// screen model itself for as long as nothing has been read after it. Before the first
    /// mark, for a program that marks no frames, and when the session was not asked to keep it
    /// ([`PtySession::keep_the_screen_of_each_mark`]), it is the screen model.
    #[must_use]
    pub fn marked_screen(&self) -> &Screen {
        match &self.marked {
            Marked::Kept(screen) => screen,
            Marked::NotKept | Marked::NoMarkYet | Marked::AtTheMark => &self.screen,
        }
    }

    /// The first mark of frame `seq` or later that the session has read. Marks come in the order
    /// of their frames, so that is the frame's own or, if that one has left the log, the oldest
    /// one kept, which was read after it: what followed that mark followed this one.
    fn mark_of(&self, seq: u64) -> Option<&MarkRead> {
        self.marks_read.iter().find(|mark| mark.seq >= seq)
    }

    /// Whether the screen model can be taken to show frame `seq` on a console host that paints on
    /// its own timer (`ConPTY`), whose mark arrives before the paint of its frame, for a read that
    /// decides nothing destructive: the mark of `seq` has been read, and then either output that is
    /// not a mark has been read after it, which is a paint taken after the host had the frame and
    /// shows that frame or a later one, or `window` has passed since the mark was read with nothing
    /// painted, which is taken to mean that nothing needed painting. Another frame's mark is not
    /// output that paints. `false` while the mark has not been read. That is a guess, and a
    /// deletion never rests on it: no paint of such a console host can be proved complete, so
    /// `runner::live` confirms no deletion from its screen. On a Unix pseudo-terminal the mark
    /// follows its frame's bytes, and [`PtySession::frame_shown`] is all there is to ask.
    #[must_use]
    pub fn paint_followed_mark(&self, seq: u64, window: Duration) -> bool {
        self.mark_of(seq)
            .is_some_and(|mark| mark.first_paint_at.is_some() || mark.at.elapsed() >= window)
    }

    /// The bytes of terminal output read so far.
    #[must_use]
    pub const fn output_bytes(&self) -> u64 {
        self.output_bytes
    }

    /// Everything the session can say about its output and the child, for a step that timed out.
    /// See [`Diagnostics`].
    #[must_use]
    pub fn diagnostics(&self) -> Diagnostics {
        Diagnostics {
            output_bytes: self.output_bytes,
            first_byte_after: self
                .first_byte_at
                .map(|at| at.saturating_duration_since(self.started)),
            exit: self.exit,
            cursor_reports_answered: self.screen.cursor_reports_answered(),
            head: self.head.clone(),
            tail: self.tail.clone(),
        }
    }

    /// Runs the command named by `EXCISE_HARNESS_DIAGNOSTIC_COMMAND` against this session's
    /// child, for a timed-out step's failure detail and the bundle's `screen.txt`. `None` when
    /// the variable is unset. See [`diagnostic_command`](super::diagnostic_command).
    #[must_use]
    pub fn sample_process(&self) -> Option<String> {
        super::diagnostic_command::sample(self.pid)
    }

    /// The resource samples of the child.
    #[must_use]
    pub const fn sampler(&self) -> &ProcessSampler {
        &self.sampler
    }

    /// How the child ended, once the session has seen it end.
    #[must_use]
    pub const fn exit(&self) -> Option<ExitInfo> {
        self.exit
    }

    /// The child's CPU time, from the change in the CPU time of all reaped children. Available
    /// after the child is reaped. See the notes in [`crate::metrics`].
    #[must_use]
    pub fn cpu_time(&self) -> Option<CpuTimes> {
        Some(self.cpu_after?.since(self.cpu_before?))
    }

    /// Whether the child has exited and every byte of its output has been read.
    #[must_use]
    pub const fn finished(&self) -> bool {
        self.exit.is_some() && self.reader_done
    }

    /// Reads whatever output is waiting, answers cursor position requests, samples the child, and
    /// notes whether it has exited. Never blocks, and takes in at most [`PUMP_LIMIT`] bytes of
    /// output: a call that finds more leaves the rest for the next one.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording or the terminal input fails, or if the terminal could
    /// not be read (once: the output that came before the failure has been taken in).
    pub fn pump(&mut self) -> Result<(), PtyError> {
        let mut taken = 0;
        while taken < PUMP_LIMIT {
            match self.chunks.try_recv() {
                Ok(Ok(chunk)) => {
                    taken += chunk.bytes.len();
                    self.backlog.leave(chunk.bytes.len());
                    self.absorb(&chunk)?;
                }
                Ok(Err(error)) => {
                    self.reader_done = true;
                    return Err(PtyError::Read(error));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.reader_done = true;
                    break;
                }
            }
        }
        self.poll_exit();
        if self.exit.is_none() && self.last_sample.elapsed() >= SAMPLE_INTERVAL {
            self.last_sample = Instant::now();
            self.sampler.sample(self.pid);
        }
        Ok(())
    }

    /// Waits up to `timeout` for terminal output and processes it. Returns early when output
    /// arrives, so a caller that loops on this call reacts as soon as the screen changes.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording or the terminal input fails, or if the terminal could
    /// not be read.
    pub fn wait_activity(&mut self, timeout: Duration) -> Result<(), PtyError> {
        if self.reader_done {
            thread::sleep(timeout);
            return Ok(());
        }
        match self.chunks.recv_timeout(timeout) {
            Ok(Ok(chunk)) => {
                self.backlog.leave(chunk.bytes.len());
                self.absorb(&chunk)
            }
            Ok(Err(error)) => {
                self.reader_done = true;
                Err(PtyError::Read(error))
            }
            Err(RecvTimeoutError::Timeout) => Ok(()),
            Err(RecvTimeoutError::Disconnected) => {
                self.reader_done = true;
                Ok(())
            }
        }
    }

    /// Keeps reading for up to `limit`, until the output has been quiet for `quiet`.
    ///
    /// This bridges the gap between an event `excise` reports and the bytes of the frame it
    /// describes, which travel by a different route: the event file is readable before the reader
    /// thread has delivered the screen bytes. It is a bounded tail after a semantic condition,
    /// never a wait for an idle screen. A terminal that can hold a frame back for longer than this
    /// tail reads (`ConPTY`) is waited for first: see `runner::live::CONPTY_FRAME_WINDOW`.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording or the terminal input fails.
    pub fn drain(&mut self, quiet: Duration, limit: Duration) -> Result<(), PtyError> {
        let deadline = Instant::now() + limit;
        let mut last_output = Instant::now();
        loop {
            let before = self.output_bytes;
            self.pump()?;
            if self.output_bytes != before {
                last_output = Instant::now();
            }
            let now = Instant::now();
            if self.reader_done
                || now.saturating_duration_since(last_output) >= quiet
                || now >= deadline
            {
                return Ok(());
            }
            self.wait_activity(quiet.min(Duration::from_millis(1)))?;
        }
    }

    /// Writes `bytes` to the terminal as input and returns the instant just before the write.
    ///
    /// # Errors
    ///
    /// Returns an error if the input is closed or the write fails.
    pub fn send(&mut self, bytes: &[u8]) -> Result<Instant, PtyError> {
        let at = Instant::now();
        self.write_input(bytes)?;
        self.record_input(at, bytes)?;
        Ok(at)
    }

    /// Resizes the terminal and the screen model, and returns when it happened. The child is told
    /// by `SIGWINCH` on Unix.
    ///
    /// Output that was already waiting is processed at the old size first.
    ///
    /// # Errors
    ///
    /// Returns an error if the terminal cannot be resized.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Result<Instant, PtyError> {
        self.pump()?;
        let at = Instant::now();
        let master = self
            .master
            .as_deref()
            .ok_or_else(|| PtyError::Resize("the pseudo-terminal was closed".to_owned()))?;
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| PtyError::Resize(format!("{error:#}")))?;
        self.screen.resize(rows, cols);
        // The copy is the size the screen was; what follows is read at the new one.
        if matches!(self.marked, Marked::Kept(_)) {
            self.marked = Marked::NoMarkYet;
        }
        if let Some((path, cast)) = &mut self.recording {
            cast.resize(at, cols, rows)
                .map_err(|source| PtyError::Recording {
                    path: path.clone(),
                    source,
                })?;
        }
        Ok(at)
    }

    /// The terminal modes now.
    ///
    /// Echo and canonical mode come from the line discipline of the pseudo-terminal, which keeps
    /// the state a process left behind even after the process is gone. That is how a process that
    /// was killed with the terminal in raw mode is told from one that restored it.
    #[must_use]
    pub fn modes(&self) -> TerminalModes {
        let ScreenModes {
            alternate_screen,
            cursor_visible,
        } = self.screen.modes();
        let (echo, icanon) = self.line_discipline();
        TerminalModes {
            alternate_screen,
            cursor_visible,
            echo,
            icanon,
        }
    }

    #[cfg(unix)]
    fn line_discipline(&self) -> (Option<bool>, Option<bool>) {
        use nix::sys::termios::LocalFlags;

        self.master
            .as_deref()
            .and_then(MasterPty::get_termios)
            .map_or((None, None), |termios| {
                (
                    Some(termios.local_flags.contains(LocalFlags::ECHO)),
                    Some(termios.local_flags.contains(LocalFlags::ICANON)),
                )
            })
    }

    #[cfg(not(unix))]
    #[allow(
        clippy::unused_self,
        reason = "the signature matches the Unix variant, which reads the line discipline"
    )]
    fn line_discipline(&self) -> (Option<bool>, Option<bool>) {
        (None, None)
    }

    /// Closes the pseudo-terminal's controlling side. Idempotent: a second call does nothing.
    ///
    /// On Windows this closes the pseudo console (`portable-pty`'s `ConPtyMasterPty` calls
    /// `ClosePseudoConsole` when its last reference is dropped, which is this one: the slave is
    /// dropped right after the child is spawned), which Windows itself delivers to every process
    /// attached to the console as `CTRL_CLOSE_EVENT`. No Windows console call is made here or
    /// needed: dropping the handle is enough. This is the `signal` step's `close` event
    /// (`runner::plan::supported_signal`); `break` needs `GenerateConsoleCtrlEvent`, which does
    /// need an unsafe call this workspace does not allow, so `signal` does not support it.
    ///
    /// On Unix `signal` never selects `close` (`supported_signal` refuses it there), so a
    /// scenario can never reach this; closing the master side here only closes the pseudo-terminal
    /// (resizing and reading the line discipline stop working, same as after any close), which is
    /// not what a Windows console-close event would mean in the first place.
    pub fn close_console(&mut self) {
        drop(self.master.take());
    }

    /// Whether [`PtySession::close_console`] has closed the console. Nothing the program writes
    /// afterwards reaches the screen model, so its terminal modes can no longer be observed.
    #[must_use]
    pub const fn console_closed(&self) -> bool {
        self.master.is_none()
    }

    /// Ends the child and everything it started, and reaps it. Does nothing once the child has
    /// been reaped.
    ///
    /// Gives up waiting for the child to die after five seconds, and says so: the result is
    /// whether the child has been reaped. `false` is a child that the kill did not reach in that
    /// time, one stuck where a signal does not take effect (a call that the file system under it
    /// does not answer), and it is still running. Also lifts any drain cap
    /// (`SpawnSpec::drain_bytes_per_sec`) immediately, so output already queued in the
    /// pseudo-terminal drains at full speed before the kill signal arrives.
    pub fn kill(&mut self) -> bool {
        // Lifted first: a process that still has output queued when its terminal closes keeps
        // writing until it is read, so the reader must drain freely before the signal arrives, or
        // the reap could stall behind a write the reader is pacing.
        self.uncapped.store(true, Ordering::Relaxed);
        if self.reaped {
            return true;
        }
        #[cfg(unix)]
        {
            let _ = crate::safety::kill_process_group(self.pid);
        }
        #[cfg(not(unix))]
        {
            let _ = self.child.kill();
        }
        let deadline = Instant::now() + KILL_TIMEOUT;
        while !self.reaped && Instant::now() < deadline {
            self.poll_exit();
            if !self.reaped {
                thread::sleep(Duration::from_millis(2));
            }
        }
        self.reaped
    }

    /// Flushes the recording. Called before the recording is copied anywhere.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording cannot be written.
    pub fn finish_recording(&mut self) -> Result<(), PtyError> {
        if let Some((path, cast)) = &mut self.recording {
            cast.finish(Instant::now())
                .map_err(|source| PtyError::Recording {
                    path: path.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Takes one read of the output in. The recording, the diagnostics, and the byte count see the
    /// bytes as they came, marks included; the screen model sees them without the marks, and the
    /// session notes which frame the screen now shows (see [`MarkScanner`]).
    fn absorb(&mut self, chunk: &Chunk) -> Result<(), PtyError> {
        self.output_bytes += chunk.bytes.len() as u64;
        if !chunk.bytes.is_empty() {
            self.first_byte_at.get_or_insert(chunk.at);
            capture_head(&mut self.head, &chunk.bytes);
            capture_tail(&mut self.tail, &chunk.bytes);
        }
        if let Some((path, cast)) = &mut self.recording {
            cast.output(chunk.at, &chunk.bytes)
                .map_err(|source| PtyError::Recording {
                    path: path.clone(),
                    source,
                })?;
        }
        let mut replies = Vec::new();
        let (screen, frame_shown, unpainted, marks_read) = (
            &mut self.screen,
            &mut self.frame_shown,
            &mut self.unpainted,
            &mut self.marks_read,
        );
        let marked = &mut self.marked;
        self.marks.feed(&chunk.bytes, |piece| match piece {
            Piece::Bytes(bytes) => {
                // The first output after a mark begins the next frame's redraw, and the screen
                // model holds that from here on: what the mark left is kept before it is fed.
                if matches!(marked, Marked::AtTheMark) {
                    *marked = Marked::Kept(Box::new(screen.snapshot()));
                }
                replies.extend_from_slice(&screen.process(bytes));
                // The marks that no output had followed are followed by this: it is what the
                // console host painted after them.
                for mark in marks_read.iter_mut().rev().take(*unpainted) {
                    mark.first_paint_at = Some(chunk.at);
                }
                *unpainted = 0;
            }
            // Frames are numbered in the order they are drawn, so a mark can only move the frame
            // the screen shows forward.
            Piece::Frame(seq) => {
                *frame_shown = (*frame_shown).max(seq);
                // Every byte of the frame is in the screen model, and nothing after it is.
                if !matches!(marked, Marked::NotKept) {
                    *marked = Marked::AtTheMark;
                }
                if marks_read.len() == MARKS_KEPT {
                    marks_read.pop_front();
                    *unpainted = (*unpainted).min(marks_read.len());
                }
                marks_read.push_back(MarkRead {
                    seq,
                    at: chunk.at,
                    first_paint_at: None,
                });
                *unpainted += 1;
            }
        });
        if !replies.is_empty() {
            // A terminal answers a cursor position request; ConPTY, for one, waits for it before it
            // shows anything. A program that has already closed its side of the terminal cannot
            // read the answer, and a terminal does not fail because an answer went unread.
            match self.write_input(&replies) {
                Ok(()) => self.record_input(Instant::now(), &replies)?,
                Err(PtyError::Input(error)) if program_side_closed(&error) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn write_input(&mut self, bytes: &[u8]) -> Result<(), PtyError> {
        let writer = self.writer.as_mut().ok_or(PtyError::InputClosed)?;
        writer.write_all(bytes).map_err(PtyError::Input)?;
        writer.flush().map_err(PtyError::Input)
    }

    fn record_input(&mut self, at: Instant, bytes: &[u8]) -> Result<(), PtyError> {
        if let Some((path, cast)) = &mut self.recording {
            cast.input(at, bytes)
                .map_err(|source| PtyError::Recording {
                    path: path.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Notes that the child exited, if it did, and reaps it.
    #[cfg(unix)]
    fn poll_exit(&mut self) {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

        if self.reaped {
            return;
        }
        let Some(pid) = i32::try_from(self.pid).ok().and_then(Pid::from_raw) else {
            return;
        };
        let options = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
        if let Ok(Some(status)) = waitid(WaitId::Pid(pid), options) {
            if self.exit.is_none() {
                self.exit = Some(ExitInfo {
                    code: status.exit_status(),
                    signal: status.terminating_signal(),
                    at: Instant::now(),
                });
                // The child is a zombie until it is reaped. Its final figures are still readable.
                self.sampler.sample(self.pid);
                if self.cgroup_wrapped {
                    self.cgroup_peak_bytes = crate::safety::cgroup::read_memory_peak(self.pid);
                }
                // The leader is a zombie until it is reaped below, so its id still names its
                // process group and cannot name another one. Whatever it started and left running
                // ends with it: a session never leaks a process, whether the child is ended by
                // `kill` or ends by itself. (There is no group to signal where there is no
                // `waitid`: see `safety::process`.)
                let _ = crate::safety::kill_process_group(self.pid);
            }
            let _ = self.child.wait();
            self.reaped = true;
            self.cpu_after = reaped_children_cpu();
        }
    }

    /// Notes that the child exited, if it did. Polling reaps it.
    #[cfg(not(unix))]
    fn poll_exit(&mut self) {
        if self.reaped {
            return;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exit = Some(ExitInfo {
                code: i32::try_from(status.exit_code()).ok(),
                signal: None,
                at: Instant::now(),
            });
            self.reaped = true;
            self.cpu_after = reaped_children_cpu();
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        self.kill();
        // The child is dead, so the end-of-file the writer sends on drop reaches nobody.
        drop(self.writer.take());
        // A reader that waits for room in the queue ends when the queue is closed, so it is
        // closed before the thread is waited for: a program that was held back by a full queue
        // does not make this wait for the whole of the join bound.
        self.backlog.close();
        if let Some(handle) = self.reader.take() {
            let deadline = Instant::now() + READER_JOIN_TIMEOUT;
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(2));
            }
            if handle.is_finished() {
                let _ = handle.join();
            }
        }
        let _ = self.finish_recording();
    }
}

/// The output that has been read from the terminal and not taken in yet: how much of it there is,
/// and whether anybody is going to take it in. The reader thread adds what it reads and waits while
/// [`QUEUE_BYTES`] wait; the session takes chunks in and says so.
#[derive(Debug, Default)]
struct Backlog {
    waiting: AtomicUsize,
    closed: AtomicBool,
}

impl Backlog {
    /// Waits until fewer than [`QUEUE_BYTES`] wait to be taken in, the bound is lifted
    /// (`uncapped`, which [`PtySession::kill`] sets), or the queue is closed. Returns whether the
    /// thread may read on: `false` once nobody is going to take the output in.
    ///
    /// The bound is lifted by the kill for the reason the pace of a capped reader is: a process
    /// that is blocked in a write to the terminal does not die until the write goes through, so a
    /// reader that held the output back would stall the reap that the kill waits for. What a
    /// program that is being killed can still write is finite.
    fn wait_for_room(&self, uncapped: &AtomicBool) -> bool {
        while self.waiting.load(Ordering::Acquire) >= QUEUE_BYTES
            && !uncapped.load(Ordering::Relaxed)
        {
            if self.closed.load(Ordering::Acquire) {
                return false;
            }
            thread::sleep(BACKLOG_POLL_INTERVAL);
        }
        !self.closed.load(Ordering::Acquire)
    }

    /// A read of `bytes` bytes is queued.
    fn join(&self, bytes: usize) {
        self.waiting.fetch_add(bytes, Ordering::AcqRel);
    }

    /// A chunk of `bytes` bytes was taken in. A chunk that was not counted in (one that a test
    /// put in the queue itself) leaves the count at zero and no lower.
    fn leave(&self, bytes: usize) {
        let _ = self
            .waiting
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |waiting| {
                Some(waiting.saturating_sub(bytes))
            });
    }

    /// Nobody is going to take the output in.
    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }
}

/// A token-bucket limit on how fast [`spawn_reader`] drains the terminal's output, so a scenario
/// can simulate a slow terminal and the backpressure it puts on the child's writes, without
/// distorting timing with a sleep per chunk: tokens accrue continuously at `rate_per_sec` and one
/// read takes only the tokens already banked.
struct DrainCap {
    rate_per_sec: u64,
    tokens: f64,
    last: Instant,
}

impl DrainCap {
    const fn new(rate_per_sec: u64, now: Instant) -> Self {
        Self {
            rate_per_sec,
            // Starts empty, not pre-filled: a fresh cap paces its very first read exactly like
            // every later one, with no one-time startup burst to account for.
            tokens: 0.0,
            last: now,
        }
    }

    /// Bytes available to read right now, at most `want`, after banking tokens for the time
    /// elapsed since the last call. Banked tokens never exceed one second's worth, so a long
    /// pause (the reader asleep, or the program not writing) is never spent later as one large
    /// burst. Invariant: `tokens` never goes negative, so `want`'s lower bound needs no clamp.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "drain rates and chunk sizes stay far below 2^52; `tokens` stays in \
                  [0.0, rate_per_sec] by construction, so the truncating cast back to a byte \
                  count is never negative"
    )]
    fn take(&mut self, want: usize, now: Instant) -> usize {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens =
            (self.tokens + elapsed * self.rate_per_sec as f64).min(self.rate_per_sec as f64);
        let allowed = self.tokens.min(want as f64);
        self.tokens -= allowed;
        allowed as usize
    }
}

/// Starts the thread that forwards terminal output. It ends when the terminal reports the end of
/// the output, which happens once the child and everything it started has closed the terminal.
///
/// `drain_bytes_per_sec` paces the reads with a [`DrainCap`], so a scenario can simulate a slow
/// terminal (see [`SpawnSpec::drain_bytes_per_sec`]). `uncapped` lifts the pace the moment
/// [`PtySession::kill`] is called, rechecked at least every [`DRAIN_CAP_POLL_INTERVAL`] even while
/// no tokens are banked, so a sleeping reader notices the kill promptly and drains the rest of the
/// output at full speed.
fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    sender: mpsc::Sender<io::Result<Chunk>>,
    drain_bytes_per_sec: Option<u64>,
    uncapped: Arc<AtomicBool>,
    backlog: Arc<Backlog>,
) -> Result<JoinHandle<()>, PtyError> {
    thread::Builder::new()
        .name("harness-pty-reader".to_owned())
        .spawn(move || {
            let mut buffer = vec![0_u8; READ_CHUNK];
            let mut cap = drain_bytes_per_sec.map(|rate| DrainCap::new(rate, Instant::now()));
            loop {
                // The output that waits is bounded: no more is read from the terminal while the
                // session has a backlog, which holds the program back in its own writes.
                if !backlog.wait_for_room(&uncapped) {
                    break;
                }
                let want = match &mut cap {
                    Some(cap) if !uncapped.load(Ordering::Relaxed) => {
                        let mut allowed = cap.take(READ_CHUNK, Instant::now());
                        while allowed == 0 && !uncapped.load(Ordering::Relaxed) {
                            thread::sleep(DRAIN_CAP_POLL_INTERVAL);
                            allowed = cap.take(READ_CHUNK, Instant::now());
                        }
                        if uncapped.load(Ordering::Relaxed) {
                            READ_CHUNK
                        } else {
                            allowed
                        }
                    }
                    _ => READ_CHUNK,
                };
                match reader.read(&mut buffer[..want]) {
                    Ok(0) => break,
                    Ok(read) => {
                        let chunk = Chunk {
                            at: Instant::now(),
                            bytes: buffer[..read].to_vec(),
                        };
                        backlog.join(read);
                        if sender.send(Ok(chunk)).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    // The end of the output: the program's side of the terminal is closed.
                    Err(error) if program_side_closed(&error) => break,
                    // Any other failure is not the end of the output, and a reader that ended
                    // without saying so would let the session believe it had read everything.
                    Err(error) => {
                        let _ = sender.send(Err(error));
                        break;
                    }
                }
            }
        })
        .map_err(|error| PtyError::Open(format!("cannot start the reader thread: {error}")))
}

/// Whether a read or a write of the terminal failed because the program's side of it is closed, so
/// that there is nothing left to read, or nothing left to read what was written: `EIO` on Unix, a
/// broken pipe on Windows.
fn program_side_closed(error: &io::Error) -> bool {
    #[cfg(unix)]
    if error.raw_os_error() == Some(rustix::io::Errno::IO.raw_os_error()) {
        return true;
    }
    error.kind() == io::ErrorKind::BrokenPipe
}

/// Keeps the first [`DIAGNOSTIC_BYTES`] of output seen, across any number of calls.
fn capture_head(head: &mut Vec<u8>, bytes: &[u8]) {
    let room = DIAGNOSTIC_BYTES.saturating_sub(head.len());
    head.extend_from_slice(&bytes[..bytes.len().min(room)]);
}

/// Keeps the last [`DIAGNOSTIC_BYTES`] of output seen, across any number of calls.
fn capture_tail(tail: &mut Vec<u8>, bytes: &[u8]) {
    if bytes.len() >= DIAGNOSTIC_BYTES {
        tail.clear();
        tail.extend_from_slice(&bytes[bytes.len() - DIAGNOSTIC_BYTES..]);
        return;
    }
    tail.extend_from_slice(bytes);
    let excess = tail.len().saturating_sub(DIAGNOSTIC_BYTES);
    tail.drain(..excess);
}

fn open_recording(
    path: &Path,
    spec: &SpawnSpec,
    started: Instant,
) -> Result<CastWriter<io::BufWriter<std::fs::File>>, PtyError> {
    let header = CastHeader {
        width: spec.cols,
        height: spec.rows,
        timestamp: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs()),
        env: spec
            .env
            .iter()
            .filter(|(name, _)| name == "TERM" || name == "COLORTERM")
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect(),
        title: spec.title.clone(),
    };
    CastWriter::create(path, &header, started).map_err(|source| PtyError::Recording {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::metrics::live_cpu_ms;
    use crate::safety::process_group_exists;

    fn shell(script: &str) -> SpawnSpec {
        SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script.into()],
            env: vec![
                ("TERM".into(), "xterm-256color".into()),
                ("PATH".into(), "/usr/bin:/bin".into()),
            ],
            cwd: std::env::temp_dir(),
            cols: 80,
            rows: 24,
            drain_bytes_per_sec: None,
            recording: None,
            title: None,
        }
    }

    /// Drives the session until `done` holds or a generous limit passes.
    fn run_until(session: &mut PtySession, done: impl Fn(&PtySession) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            session.pump().expect("pump");
            if done(session) || Instant::now() > deadline {
                return;
            }
            session
                .wait_activity(Duration::from_millis(2))
                .expect("wait");
        }
    }

    fn gone(pid: u32) -> bool {
        use rustix::process::{Pid, test_kill_process};

        let pid = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .expect("a pid");
        test_kill_process(pid).is_err()
    }

    fn eventually(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        condition()
    }

    #[test]
    fn output_reaches_the_screen_and_the_exit_code_is_reported() {
        let mut session =
            PtySession::spawn(&shell("printf 'hello screen'; exit 3")).expect("spawn");

        run_until(&mut session, PtySession::finished);

        assert!(session.screen().text().contains("hello screen"));
        let exit = session.exit().expect("the child exited");
        assert_eq!((exit.code, exit.signal), (Some(3), None));
        assert_eq!(exit.describe(), "exit code 3");
        assert!(session.output_bytes() >= 12);
    }

    #[test]
    fn a_relative_program_is_refused() {
        let mut spec = shell("true");
        spec.program = PathBuf::from("sh");

        assert!(matches!(
            PtySession::spawn(&spec),
            Err(PtyError::RelativeProgram(_))
        ));
    }

    #[test]
    fn a_missing_program_is_a_spawn_error() {
        let mut spec = shell("true");
        spec.program = PathBuf::from("/definitely/not/a/program");

        assert!(matches!(
            PtySession::spawn(&spec),
            Err(PtyError::Spawn { .. })
        ));
    }

    #[test]
    fn input_reaches_the_program() {
        let mut session =
            PtySession::spawn(&shell("read word; printf 'got:%s' \"$word\"")).expect("spawn");

        session.send(b"abc\r").expect("send");
        run_until(&mut session, PtySession::finished);

        assert!(
            session.screen().text().contains("got:abc"),
            "{}",
            session.screen().text()
        );
    }

    #[test]
    fn a_cursor_position_request_gets_the_real_cursor_position() {
        let script =
            "stty raw -echo; printf '\\033[5;7H\\033[6n'; dd bs=1 count=6 2>/dev/null | od -An -c";
        let mut session = PtySession::spawn(&shell(script)).expect("spawn");

        run_until(&mut session, PtySession::finished);

        let text = session.screen().text().replace(' ', "");
        assert!(
            text.contains("033[5;7R"),
            "the program should have read the answer: {text:?}"
        );
    }

    #[test]
    fn diagnostics_report_output_timing_cursor_reports_and_bounded_ends() {
        let mut session = PtySession::spawn(&shell(
            "stty -echo; printf '%300s' '' | tr ' ' 'A'; printf '\\033[6n'; \
             printf 'tail-marker'; exit 7",
        ))
        .expect("spawn");

        let before = session.diagnostics();
        assert_eq!(before.output_bytes, 0);
        assert!(before.first_byte_after.is_none());
        assert_eq!(before.cursor_reports_answered, 0);
        assert!(before.exit.is_none());
        assert_eq!(
            before.to_string(),
            "0 output bytes received; the program is still running; 0 cursor-position requests \
             answered; first bytes \"\", last bytes \"\""
        );

        run_until(&mut session, PtySession::finished);

        let after = session.diagnostics();
        assert!(after.output_bytes >= 315, "{after:?}");
        assert!(after.first_byte_after.is_some());
        assert_eq!(after.cursor_reports_answered, 1);
        assert_eq!(after.head.len(), DIAGNOSTIC_BYTES);
        assert!(
            after.head.iter().all(|&byte| byte == b'A'),
            "{:?}",
            after.head
        );
        assert!(
            after.tail.ends_with(b"tail-marker"),
            "{:?}",
            String::from_utf8_lossy(&after.tail)
        );
        let exit = after.exit.expect("the child exited");
        assert_eq!(exit.code, Some(7));
        assert!(
            after.to_string().contains("not running (exit code 7)"),
            "{after}"
        );
    }

    #[test]
    fn a_cursor_request_from_a_program_that_has_already_exited_is_not_an_error() {
        let mut session = PtySession::spawn(&shell("printf '\\033[6n'; exit 0")).expect("spawn");
        // Read nothing until the program has closed its side of the terminal and every byte has
        // been read, so the answer to its request can only be written after it is gone.
        let deadline = Instant::now() + Duration::from_secs(20);
        while !session.reader.as_ref().is_some_and(JoinHandle::is_finished) {
            assert!(
                Instant::now() < deadline,
                "the program never closed its terminal"
            );
            thread::sleep(Duration::from_millis(5));
        }

        run_until(&mut session, PtySession::finished);

        let diagnostics = session.diagnostics();
        assert_eq!(diagnostics.cursor_reports_answered, 1);
        assert_eq!(diagnostics.exit.expect("the child exited").code, Some(0));
    }

    #[test]
    fn the_terminal_modes_report_the_display_and_the_line_discipline() {
        let mut session = PtySession::spawn(&shell(
            "printf '\\033[?1049h\\033[?25l'; stty raw -echo; sleep 30",
        ))
        .expect("spawn");

        run_until(&mut session, |session| session.modes().alternate_screen);
        assert!(eventually(|| {
            session.pump().expect("pump");
            session.modes().echo == Some(false)
        }));

        let modes = session.modes();
        assert!(modes.alternate_screen && !modes.cursor_visible);
        assert_eq!((modes.echo, modes.icanon), (Some(false), Some(false)));
        assert!(!modes.restored());
    }

    #[test]
    fn a_program_that_leaves_the_terminal_alone_is_restored() {
        let mut session = PtySession::spawn(&shell("printf x")).expect("spawn");

        run_until(&mut session, PtySession::finished);

        let modes = session.modes();
        assert_eq!((modes.echo, modes.icanon), (Some(true), Some(true)));
        assert!(modes.restored());
    }

    #[test]
    fn a_killed_program_leaves_its_terminal_state_readable() {
        let mut session =
            PtySession::spawn(&shell("printf '\\033[?1049h'; stty raw -echo; sleep 30"))
                .expect("spawn");
        run_until(&mut session, |session| session.modes().alternate_screen);
        assert!(eventually(|| session.modes().echo == Some(false)));

        session.kill();
        session
            .drain(Duration::from_millis(20), Duration::from_millis(500))
            .expect("drain");

        let exit = session.exit().expect("the child is dead");
        assert_eq!(exit.signal, Some(9), "{exit:?}");
        assert_eq!(exit.code, None);
        assert_eq!(exit.describe(), "killed by signal 9");
        let modes = session.modes();
        assert_eq!(
            (modes.echo, modes.icanon),
            (Some(false), Some(false)),
            "raw mode outlives the process"
        );
        assert!(modes.alternate_screen);
    }

    #[test]
    fn killing_the_session_kills_everything_the_child_started() {
        let mut session = PtySession::spawn(&shell(
            "sleep 60 & echo first:$!; sleep 60 & echo second:$!; wait",
        ))
        .expect("spawn");
        run_until(&mut session, |session| {
            session.screen().text().contains("second:")
        });
        let text = session.screen().text();
        let background: Vec<u32> = text
            .lines()
            .filter_map(|line| line.split_once(':'))
            .filter_map(|(_, pid)| pid.trim().parse().ok())
            .collect();
        assert_eq!(background.len(), 2, "two background sleepers: {text:?}");
        let group = session.pid();
        assert!(process_group_exists(group));

        session.kill();

        assert!(session.exit().is_some_and(|exit| exit.signal == Some(9)));
        assert!(
            eventually(|| !process_group_exists(group)),
            "the process group must be empty after the kill"
        );
        for pid in background {
            assert!(
                eventually(|| gone(pid)),
                "background process {pid} survived the kill"
            );
        }
    }

    #[test]
    fn dropping_a_running_session_kills_it() {
        let session = PtySession::spawn(&shell("sleep 60")).expect("spawn");
        let pid = session.pid();

        drop(session);

        assert!(
            eventually(|| gone(pid)),
            "the child must not outlive its session"
        );
    }

    #[test]
    fn the_recording_holds_output_and_input() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("session.cast");
        let mut spec = shell("read word; printf 'got:%s' \"$word\"");
        spec.recording = Some(path.clone());
        let mut session = PtySession::spawn(&spec).expect("spawn");

        session.send(b"abc\r").expect("send");
        run_until(&mut session, PtySession::finished);
        session.finish_recording().expect("flush");

        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .expect("the recording")
            .lines()
            .map(|line| serde_json::from_str(line).expect("JSON"))
            .collect();
        assert_eq!(lines[0]["version"], 2);
        assert_eq!(lines[0]["width"], 80);
        let kinds: Vec<(&str, &str)> = lines[1..]
            .iter()
            .map(|event| {
                (
                    event[1].as_str().expect("kind"),
                    event[2].as_str().expect("data"),
                )
            })
            .collect();
        assert!(kinds.contains(&("i", "abc\r")), "{kinds:?}");
        assert!(
            kinds
                .iter()
                .any(|(kind, data)| *kind == "o" && data.contains("got:abc")),
            "{kinds:?}"
        );
    }

    #[test]
    fn the_marks_a_program_writes_move_the_frame_the_screen_shows_and_stay_in_the_recording() {
        // The first mark is written in two pieces with a pause between them, which a read can
        // fall between; the second comes in the same write as the text around it. Where the
        // reads end must change nothing: the screen model gets the text without the marks, the
        // session knows the latest frame whose mark it read, and the recording keeps every byte
        // as the program wrote it.
        let dir = tempfile::tempdir().expect("a temporary directory");
        let path = dir.path().join("session.cast");
        let mut spec = shell(
            "printf 'one\\033]9471;excise-fr'; sleep 0.2; \
             printf 'ame=1\\007two\\033]9471;excise-frame=2\\007three'",
        );
        spec.recording = Some(path.clone());
        let mut session = PtySession::spawn(&spec).expect("spawn");
        assert_eq!(session.frame_shown(), 0, "no mark has been read");

        run_until(&mut session, PtySession::finished);
        session.finish_recording().expect("flush");

        assert_eq!(session.frame_shown(), 2);
        assert!(
            session.screen().text().contains("onetwothree"),
            "{}",
            session.screen().text()
        );
        let recorded: String = std::fs::read_to_string(&path)
            .expect("the recording")
            .lines()
            .skip(1)
            .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("JSON"))
            .filter(|event| event[1] == "o")
            .map(|event| event[2].as_str().expect("data").to_owned())
            .collect();
        for seq in [1, 2] {
            let mark = format!("\u{1b}]9471;excise-frame={seq}\u{7}");
            assert!(
                recorded.contains(&mark),
                "mark {seq} is not in {recorded:?}"
            );
        }
    }

    #[test]
    fn a_resize_reaches_the_program_and_the_screen_model() {
        let mut session = PtySession::spawn(&shell(
            "trap 'stty size' WINCH; stty size; i=0; while [ $i -lt 400 ]; do sleep 0.05; i=$((i+1)); done",
        ))
        .expect("spawn");
        run_until(&mut session, |session| {
            session.screen().text().contains("24 80")
        });

        session.resize(50, 10).expect("resize");
        run_until(&mut session, |session| {
            session.screen().text().contains("10 50")
        });

        assert_eq!(session.screen().size(), (10, 50));
        assert!(
            session.screen().text().contains("10 50"),
            "{}",
            session.screen().text()
        );
    }

    #[test]
    fn samples_are_taken_while_the_child_runs() {
        let mut session = PtySession::spawn(&shell("sleep 1")).expect("spawn");

        run_until(&mut session, PtySession::finished);

        assert!(session.sampler().samples() >= 1, "{:?}", session.sampler());
        assert!(session.cpu_time().is_some());
    }

    /// Drives the session for exactly `duration`, discarding what it sees: the control below
    /// needs a fixed window, not a condition to wait for.
    fn pump_for(session: &mut PtySession, duration: Duration) {
        let deadline = Instant::now() + duration;
        loop {
            session.pump().expect("pump");
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            session
                .wait_activity((deadline - now).min(Duration::from_millis(5)))
                .expect("wait");
        }
    }

    /// The control the idle measurement needs: a stub that is known, independently of `excise`,
    /// to write nothing and spend no CPU must read back as exactly that through the same session
    /// primitives the `idle` step uses (`output_bytes`, `live_cpu_ms`).
    #[test]
    fn a_sleeping_stub_shows_no_output_and_almost_no_cpu() {
        let mut session = PtySession::spawn(&shell("sleep 2")).expect("spawn");
        pump_for(&mut session, Duration::from_millis(50)); // let it actually start running

        let start_bytes = session.output_bytes();
        let start_cpu = live_cpu_ms(session.pid()).expect("a live sample");

        pump_for(&mut session, Duration::from_millis(800));

        let output_bytes = session.output_bytes() - start_bytes;
        let cpu_ms = live_cpu_ms(session.pid()).expect("a live sample") - start_cpu;
        session.kill();

        assert_eq!(output_bytes, 0, "a sleeping process should write nothing");
        assert!(
            cpu_ms < 50.0,
            "a sleeping process should spend almost no CPU, read {cpu_ms} ms"
        );
    }

    /// The other half of the same control: a stub that is known to spend the whole window
    /// runnable, never blocked, must read back CPU time clearly above zero.
    #[test]
    fn a_busy_looping_stub_shows_cpu_clearly_above_zero() {
        let mut session = PtySession::spawn(&shell("while :; do :; done")).expect("spawn");
        pump_for(&mut session, Duration::from_millis(50));

        let start_cpu = live_cpu_ms(session.pid()).expect("a live sample");
        pump_for(&mut session, Duration::from_millis(800));
        let cpu_ms = live_cpu_ms(session.pid()).expect("a live sample") - start_cpu;
        session.kill();

        // A sleeping stub reads back exactly 0 ms (its own test above), so any margin above 0
        // already distinguishes it from a busy one; 15 ms needs only 2% of a core over the 800 ms
        // window, generous headroom on a host whose other load can leave a runnable thread far
        // short of a full core (observed 104 ms under load average 14 on 10 cores).
        assert!(
            cpu_ms > 15.0,
            "an 800 ms busy loop should accumulate real CPU time, read {cpu_ms} ms"
        );
    }

    /// `close_console` is shared, platform-independent code: on Unix it closes the pty master
    /// just like this test exercises; on Windows the same `Option::take` is what drops the last
    /// `ConPtyMasterPty` reference and triggers `ClosePseudoConsole`. This proves the Unix-reachable
    /// half is panic-free, idempotent, and that the methods which need the master degrade instead
    /// of panicking once it is gone.
    #[test]
    fn closing_the_console_is_idempotent_and_leaves_resize_and_modes_reporting_it_is_gone() {
        let mut session = PtySession::spawn(&shell("sleep 30")).expect("spawn");
        assert!(!session.console_closed());

        session.close_console();
        session.close_console();
        assert!(session.console_closed());

        assert!(
            matches!(session.resize(100, 40), Err(PtyError::Resize(_))),
            "a resize after close should report the pseudo-terminal is closed, not panic"
        );
        let modes = session.modes();
        assert_eq!(
            (modes.echo, modes.icanon),
            (None, None),
            "the line discipline cannot be read once the master is gone"
        );

        session.kill();
        assert!(
            session.exit().is_some(),
            "the child is still reaped normally"
        );
    }

    /// A reader that gives what it holds and then fails with `error`.
    struct Failing {
        held: Vec<u8>,
        error: Option<io::Error>,
    }

    impl Read for Failing {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if !self.held.is_empty() {
                let count = self.held.len().min(buffer.len());
                buffer[..count].copy_from_slice(&self.held[..count]);
                self.held.drain(..count);
                return Ok(count);
            }
            Err(self
                .error
                .take()
                .unwrap_or_else(|| io::Error::other("the terminal failed again")))
        }
    }

    #[test]
    fn a_failed_read_of_the_terminal_is_reported_and_is_not_the_end_of_the_output() {
        let (sender, chunks) = mpsc::channel();
        let reader = spawn_reader(
            Box::new(Failing {
                held: b"output".to_vec(),
                error: Some(io::Error::other("a failing terminal")),
            }),
            sender,
            None,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Backlog::default()),
        )
        .expect("a reader");

        let first = chunks
            .recv_timeout(Duration::from_secs(10))
            .expect("the output")
            .map(|chunk| chunk.bytes)
            .expect("a read");
        let failure = chunks
            .recv_timeout(Duration::from_secs(10))
            .expect("the failure");

        assert_eq!(first, b"output");
        assert!(
            matches!(&failure, Err(error) if error.to_string().contains("a failing terminal")),
            "the failure is delivered to the session"
        );
        reader.join().expect("the reader ended");
        assert!(matches!(chunks.try_recv(), Err(TryRecvError::Disconnected)));
    }

    #[test]
    fn a_terminal_whose_program_side_is_closed_ends_the_output_without_an_error() {
        for error in [
            io::Error::from(io::ErrorKind::BrokenPipe),
            io::Error::from_raw_os_error(rustix::io::Errno::IO.raw_os_error()),
        ] {
            let (sender, chunks) = mpsc::channel();
            let reader = spawn_reader(
                Box::new(Failing {
                    held: Vec::new(),
                    error: Some(error),
                }),
                sender,
                None,
                Arc::new(AtomicBool::new(false)),
                Arc::new(Backlog::default()),
            )
            .expect("a reader");

            reader.join().expect("the reader ended");

            assert!(
                matches!(chunks.try_recv(), Err(TryRecvError::Disconnected)),
                "the end of the output is not a failure"
            );
        }
    }

    #[test]
    fn a_read_error_reaches_the_caller_once_and_then_the_session_is_done_reading() {
        let mut session = PtySession::spawn(&shell("sleep 30")).expect("spawn");
        let (sender, chunks) = mpsc::channel();
        session.chunks = chunks;
        sender
            .send(Err(io::Error::other("a failing terminal")))
            .expect("sent");
        drop(sender);

        let first = session.pump();

        assert!(
            matches!(&first, Err(PtyError::Read(error)) if error.to_string().contains("failing")),
            "{first:?}"
        );
        assert!(session.pump().is_ok(), "the failure is reported once");
        session.kill();
    }

    #[test]
    fn one_pump_takes_in_a_bounded_amount_of_output_and_the_rest_waits_for_the_next() {
        // The child writes three times what one call takes in, and ends. The reader thread
        // queues what the child writes, whether or not anyone pumps (it is less than the queue
        // holds), so when the child has written it all the queue holds more than a call takes.
        const TOTAL: usize = 3 * PUMP_LIMIT;
        const _: () = assert!(TOTAL < QUEUE_BYTES);
        let dir = tempfile::Builder::new()
            .prefix("xt-pump-")
            .tempdir()
            .expect("a directory");
        let written = dir.path().join("written");
        let script = format!(
            "head -c {TOTAL} /dev/zero | tr '\\0' x; : > '{}'",
            written.display()
        );
        let mut session = PtySession::spawn(&shell(&script)).expect("spawn");
        assert!(
            eventually(|| written.exists()),
            "the child never finished writing"
        );

        session.pump().expect("pump");

        let first = session.output_bytes();
        assert!(first > 0, "the call took in nothing");
        assert!(
            first <= (PUMP_LIMIT + READ_CHUNK) as u64,
            "one call took in {first} bytes of {TOTAL}"
        );
        run_until(&mut session, PtySession::finished);
        assert!(
            session.output_bytes() >= TOTAL as u64,
            "what one call left was taken in by the next: {} of {TOTAL}",
            session.output_bytes()
        );
    }

    #[test]
    fn a_program_that_writes_more_than_the_queue_holds_is_held_back_until_the_session_reads() {
        // The child writes twice what the queue holds. Nothing pumps, so the reader thread stops
        // reading when the queue is full, the terminal fills, and the child waits in its own
        // write: it cannot get to the file it makes when it has written everything. Without the
        // bound the thread would queue all of it, and the child would end at once.
        const TOTAL: usize = 2 * QUEUE_BYTES;
        let dir = tempfile::Builder::new()
            .prefix("xt-held-")
            .tempdir()
            .expect("a directory");
        let written = dir.path().join("written");
        let script = format!(
            "head -c {TOTAL} /dev/zero | tr '\\0' x; : > '{}'",
            written.display()
        );
        let mut session = PtySession::spawn(&shell(&script)).expect("spawn");

        assert!(
            eventually(|| session.backlog.waiting.load(Ordering::Acquire) >= QUEUE_BYTES),
            "the queue never filled"
        );
        thread::sleep(Duration::from_millis(500));
        let waiting = session.backlog.waiting.load(Ordering::Acquire);
        assert!(
            waiting <= QUEUE_BYTES + READ_CHUNK,
            "{waiting} bytes waited to be read, more than the bound of {QUEUE_BYTES}"
        );
        assert!(
            !written.exists(),
            "the child wrote {TOTAL} bytes that nobody had read"
        );
        run_until(&mut session, PtySession::finished);
        assert!(written.exists(), "the child never finished writing");
        assert!(
            session.output_bytes() >= TOTAL as u64,
            "what was held back was read once the session pumped: {} of {TOTAL}",
            session.output_bytes()
        );
    }

    #[test]
    fn dropping_a_session_whose_reader_waits_for_room_does_not_wait_for_the_reader() {
        // The queue is full and nobody reads: the reader thread is in its wait for room, and the
        // child is in its write. Dropping the session ends the child and closes the queue, and
        // the thread ends at once instead of at the end of the bound on the join.
        const TOTAL: usize = 2 * QUEUE_BYTES;
        let session = PtySession::spawn(&shell(&format!(
            "head -c {TOTAL} /dev/zero | tr '\\0' x; sleep 30"
        )))
        .expect("spawn");
        assert!(
            eventually(|| session.backlog.waiting.load(Ordering::Acquire) >= QUEUE_BYTES),
            "the queue never filled"
        );
        thread::sleep(Duration::from_millis(200));

        let started = Instant::now();
        drop(session);

        assert!(
            started.elapsed() < READER_JOIN_TIMEOUT,
            "the drop waited {:?} for a reader that was only waiting for room",
            started.elapsed()
        );
    }

    /// A chunk of terminal output, as the reader thread would have read it.
    fn chunk(bytes: &[u8]) -> Chunk {
        Chunk {
            at: Instant::now(),
            bytes: bytes.to_vec(),
        }
    }

    #[test]
    fn the_screen_of_a_mark_is_kept_while_the_start_of_the_next_frame_is_read() {
        // One read ends in the middle of the redraw after a mark, which is where a read ends as
        // often as not. The screen model then holds half of the next frame, and the screen as the
        // mark left it is the copy made when that output came.
        let mut session = PtySession::spawn(&shell("sleep 30")).expect("spawn");
        session.keep_the_screen_of_each_mark();

        session
            .absorb(&chunk(
                b"\x1b[2J\x1b[1;1Hframe one\x1b]9471;excise-frame=1\x07\x1b[2J\x1b[1;1Hframe t",
            ))
            .expect("a read");

        assert_eq!(session.frame_shown(), 1);
        assert_eq!(session.screen().row_text(0), "frame t", "half of frame two");
        assert_eq!(session.marked_screen().row_text(0), "frame one");

        // The rest of frame two comes, and its mark: the screen model is that frame's again.
        session
            .absorb(&chunk(b"wo\x1b]9471;excise-frame=2\x07"))
            .expect("a read");

        assert_eq!(session.frame_shown(), 2);
        assert_eq!(session.screen().row_text(0), "frame two");
        assert_eq!(session.marked_screen().row_text(0), "frame two");

        // A later read, with the start of frame three in it, leaves the mark's screen as it was.
        session
            .absorb(&chunk(b"\x1b[2J\x1b[1;1Hframe th"))
            .expect("a read");

        assert_eq!(session.screen().row_text(0), "frame th");
        assert_eq!(session.marked_screen().row_text(0), "frame two");
        session.kill();
    }

    #[test]
    fn a_session_that_was_not_asked_keeps_no_copy_and_its_marked_screen_is_the_screen_model() {
        let mut session = PtySession::spawn(&shell("sleep 30")).expect("spawn");

        session
            .absorb(&chunk(
                b"\x1b[2J\x1b[1;1Hframe one\x1b]9471;excise-frame=1\x07\x1b[2J\x1b[1;1Hframe t",
            ))
            .expect("a read");

        assert_eq!(session.marked_screen().row_text(0), "frame t");
        session.kill();
    }

    #[test]
    fn before_the_first_mark_the_marked_screen_is_the_screen_model() {
        let mut session = PtySession::spawn(&shell("sleep 30")).expect("spawn");
        session.keep_the_screen_of_each_mark();

        session
            .absorb(&chunk(b"\x1b[2J\x1b[1;1Hstarting"))
            .expect("a read");

        assert_eq!(session.frame_shown(), 0);
        assert_eq!(session.marked_screen().row_text(0), "starting");
        session.kill();
    }

    #[test]
    fn what_a_child_started_does_not_outlive_it() {
        // The child starts a sleeper that ignores the hang-up the terminal sends to its group when
        // its leader ends, and the child ends at once. Nothing but the session ends the sleeper.
        let mut session =
            PtySession::spawn(&shell("trap '' HUP; sleep 60 & echo background:$!; exit 0"))
                .expect("spawn");

        run_until(&mut session, PtySession::finished);

        assert!(
            session.finished(),
            "the output did not end: {}",
            session.screen().text()
        );
        let text = session.screen().text();
        let sleeper: u32 = text
            .lines()
            .find_map(|line| line.strip_prefix("background:"))
            .and_then(|pid| pid.trim().parse().ok())
            .expect("the process id of the sleeper");
        assert!(
            eventually(|| gone(sleeper)),
            "the sleeper outlived the child that started it"
        );
        assert!(
            eventually(|| !process_group_exists(session.pid())),
            "the process group must be empty once the child has ended"
        );
    }
}

#[cfg(test)]
mod drain_cap_tests {
    use std::time::{Duration, Instant};

    use super::DrainCap;

    /// The cap's throughput over a simulated burst stays near its configured rate, with a
    /// generous tolerance. Time is synthetic (`start + Duration::from_millis(..)`), never
    /// `thread::sleep` or a real elapsed `Instant::now` delta, so the comparison is exact and
    /// cannot flake under machine load.
    #[test]
    #[allow(
        clippy::cast_precision_loss,
        reason = "the byte counts here stay far below 2^52"
    )]
    fn a_capped_readers_throughput_stays_near_its_rate_over_a_simulated_burst() {
        const RATE_BYTES_PER_SEC: u64 = 1000;
        const STEPS: u32 = 100;
        const STEP_MS: u64 = 100;
        let start = Instant::now();
        let mut cap = DrainCap::new(RATE_BYTES_PER_SEC, start);
        let mut taken = 0_usize;
        for step in 1..=STEPS {
            let now = start + Duration::from_millis(u64::from(step) * STEP_MS);
            taken += cap.take(10_000, now);
        }
        let elapsed_secs = f64::from(STEPS) * (STEP_MS as f64 / 1000.0);
        let observed_rate = taken as f64 / elapsed_secs;
        let rate = RATE_BYTES_PER_SEC as f64;
        let tolerance = 0.05;
        assert!(
            (observed_rate - rate).abs() <= rate * tolerance,
            "observed {observed_rate:.1} bytes/s over simulated time, wanted within \
             {:.0}% of {rate}",
            tolerance * 100.0,
        );
    }

    /// A request for no more than the tokens banked by elapsed time is granted in full, never
    /// throttled below what it asked for.
    #[test]
    fn a_request_within_the_banked_tokens_is_granted_in_full() {
        let start = Instant::now();
        let mut cap = DrainCap::new(1_000_000, start);
        // A whole second elapses, banking exactly the rate; a smaller request is granted in full.
        let later = start + Duration::from_secs(1);
        assert_eq!(cap.take(4_096, later), 4_096);
    }
}
