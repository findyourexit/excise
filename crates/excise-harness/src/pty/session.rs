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
//! without reaping (`waitid` with `WNOWAIT`), so its final resource figures can still be read.
//!
//! The library's writer sends a newline and an end-of-file character to the child when it is
//! dropped. That input could confirm a dialog, so the session drops the writer only once the child
//! is dead.
//!
//! # Windows
//!
//! The child is a `ConPTY` process. It is ended with `TerminateProcess`, exits are found by polling,
//! and the terminal's echo and canonical modes are unavailable. See `safety::process`.

use std::{
    ffi::OsString,
    fmt,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
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
    screen::{Screen, ScreenModes},
};

/// How often a running child is sampled for memory, threads, and descriptors.
const SAMPLE_INTERVAL: Duration = Duration::from_millis(50);
/// How long [`PtySession::kill`] waits for a killed child to die before it gives up.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);
/// How long dropping a session waits for the reader thread to finish.
const READER_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// The size of one read from the terminal.
const READ_CHUNK: usize = 64 * 1024;
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
    chunks: Receiver<Chunk>,
    reader: Option<JoinHandle<()>>,
    reader_done: bool,
    /// Lifted by [`PtySession::kill`] so a capped reader drains freely once the run ends.
    uncapped: Arc<AtomicBool>,
    screen: Screen,
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
        let reader = spawn_reader(
            reader,
            sender,
            spec.drain_bytes_per_sec,
            Arc::clone(&uncapped),
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
            reader: Some(reader),
            reader_done: false,
            uncapped,
            screen: Screen::new(spec.rows, spec.cols),
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
    /// notes whether it has exited. Never blocks.
    ///
    /// # Errors
    ///
    /// Returns an error if the recording or the terminal input fails.
    pub fn pump(&mut self) -> Result<(), PtyError> {
        loop {
            match self.chunks.try_recv() {
                Ok(chunk) => self.absorb(&chunk)?,
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
    /// Returns an error if the recording or the terminal input fails.
    pub fn wait_activity(&mut self, timeout: Duration) -> Result<(), PtyError> {
        if self.reader_done {
            thread::sleep(timeout);
            return Ok(());
        }
        match self.chunks.recv_timeout(timeout) {
            Ok(chunk) => self.absorb(&chunk),
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
    /// never a wait for an idle screen.
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
    /// Gives up waiting for the child to die after five seconds. Also lifts any drain cap
    /// (`SpawnSpec::drain_bytes_per_sec`) immediately, so output already queued in the
    /// pseudo-terminal drains at full speed before the kill signal arrives.
    pub fn kill(&mut self) {
        // Lifted first: a process that still has output queued when its terminal closes keeps
        // writing until it is read, so the reader must drain freely before the signal arrives, or
        // the reap could stall behind a write the reader is pacing.
        self.uncapped.store(true, Ordering::Relaxed);
        if self.reaped {
            return;
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
        let replies = self.screen.process(&chunk.bytes);
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
    sender: mpsc::Sender<Chunk>,
    drain_bytes_per_sec: Option<u64>,
    uncapped: Arc<AtomicBool>,
) -> Result<JoinHandle<()>, PtyError> {
    thread::Builder::new()
        .name("harness-pty-reader".to_owned())
        .spawn(move || {
            let mut buffer = vec![0_u8; READ_CHUNK];
            let mut cap = drain_bytes_per_sec.map(|rate| DrainCap::new(rate, Instant::now()));
            loop {
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
                        if sender.send(chunk).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| PtyError::Open(format!("cannot start the reader thread: {error}")))
}

/// Whether a write to the terminal failed because the program's side of it is closed, so nothing
/// is left to read what was written: `EIO` on Unix, a broken pipe on Windows.
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
