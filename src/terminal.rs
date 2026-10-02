use std::io::{self, IsTerminal as _};
use std::panic::{self, PanicHookInfo};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use crossterm::cursor::Show;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::queue;
use crossterm::style::ResetColor;
use crossterm::terminal::{
    DisableLineWrap, EnableLineWrap, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use thiserror::Error;

use crate::error::AppError;

type PanicHook = Box<dyn for<'a> Fn(&PanicHookInfo<'a>) + Send + Sync + 'static>;

/// Writes terminal output while splitting the paired truecolour SGR command
/// emitted by `CrosstermBackend`. Each colour command is otherwise unchanged.
/// Some terminal renderers parse the foreground half but display the paired
/// background parameters as text, so equivalent sequential commands are safer.
pub(crate) struct SplitColorWriter<W> {
    inner: W,
    pending_csi: Vec<u8>,
}

impl<W> SplitColorWriter<W> {
    pub(crate) fn new(inner: W) -> Self {
        Self {
            inner,
            pending_csi: Vec::with_capacity(48),
        }
    }

    fn write_pending_csi(&mut self) -> io::Result<()>
    where
        W: io::Write,
    {
        if let Some(background_start) = combined_truecolour_background_start(&self.pending_csi) {
            // The separator before the background belongs to neither new
            // command: terminate the foreground, then begin a new CSI.
            self.inner
                .write_all(&self.pending_csi[..background_start - 1])?;
            self.inner.write_all(b"m\x1b[")?;
            self.inner
                .write_all(&self.pending_csi[background_start..])?;
        } else {
            self.inner.write_all(&self.pending_csi)?;
        }
        self.pending_csi.clear();
        Ok(())
    }
}

impl<W> io::Write for SplitColorWriter<W>
where
    W: io::Write,
{
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut offset = 0;
        while offset < buffer.len() {
            if self.pending_csi.is_empty() {
                let Some(escape_offset) = buffer[offset..].iter().position(|&byte| byte == b'\x1b')
                else {
                    self.inner.write_all(&buffer[offset..])?;
                    break;
                };
                let escape = offset + escape_offset;
                self.inner.write_all(&buffer[offset..escape])?;
                self.pending_csi.push(b'\x1b');
                offset = escape + 1;
                continue;
            }

            self.pending_csi.push(buffer[offset]);
            offset += 1;
            let complete_csi = self.pending_csi.len() > 2
                && self
                    .pending_csi
                    .last()
                    .is_some_and(|byte| (b'@'..=b'~').contains(byte));
            if (self.pending_csi.len() == 2 && self.pending_csi[1] != b'[')
                || complete_csi
                || self.pending_csi.len() >= 64
            {
                self.write_pending_csi()?;
            }
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.write_pending_csi()?;
        self.inner.flush()
    }
}

/// Locates the truecolour background parameter after a complete truecolour
/// foreground parameter. Counting the three foreground components prevents a
/// red component with the value `48` from being mistaken for a background SGR.
fn combined_truecolour_background_start(sequence: &[u8]) -> Option<usize> {
    let mut remainder = sequence.strip_prefix(b"\x1b[38;2;")?;
    for _ in 0..3 {
        let delimiter = remainder.iter().position(|&byte| byte == b';')?;
        remainder = &remainder[delimiter + 1..];
    }
    remainder
        .starts_with(b"48;2;")
        .then_some(sequence.len() - remainder.len())
}

/// How often [`FrameSink::wait_drained`] rechecks whether the terminal writer thread has caught
/// up, while waiting out a bounded drain at terminal restoration.
const DRAIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// How long [`TerminalSession::restore`] and the panic hook wait for the terminal writer thread
/// to drain every byte handed to it (frames plus the restoration sequence) before giving up and
/// returning anyway.
///
/// A worst-case full-screen redraw at 120x40 is a few hundred kilobytes of ANSI (cursor moves,
/// colour commands, glyphs) even without a coalescing opportunity; at the slowest terminal excise
/// is required to stay independent of (150 KB/s), draining one such frame takes a little over
/// three seconds. This bound gives that legitimate case comfortable headroom while still giving
/// up on a terminal that genuinely never reads anything, so a hung terminal cannot hang exit.
const RESTORE_DRAIN_BOUND: Duration = Duration::from_secs(5);

/// Thread name for the dedicated terminal-output writer ([`spawn_frame_writer`]).
const FRAME_WRITER_THREAD_NAME: &str = "excise-terminal-writer";

/// State shared between [`FrameWriter`] (the owner loop's `io::Write` handle) and every
/// [`FrameSink`] clone (the render gate, and the restoration path).
struct DrainState {
    /// Count of chunks handed to the writer thread that have not yet finished writing (or been
    /// dropped on the failure path). The enqueuing thread increments this strictly before
    /// sending its chunk; the writer thread decrements it strictly after that same chunk's write
    /// (or discard) completes. Every increment has exactly one matching decrement, both ordered
    /// around that one chunk's own send/receive rather than around "is the queue empty right
    /// now": there is no window where one thread's "nothing left to drain" read can race another
    /// thread's "something was just queued" write, unlike a `bool` cleared by checking emptiness
    /// after the fact. Zero means every chunk produced so far has been absorbed by the terminal
    /// (or discarded because delivery is impossible): the owner loop may render a new frame, and
    /// a bounded wait may stop polling.
    pending: AtomicUsize,
    /// The first write error the background thread hit, if any, taken and surfaced the next time
    /// a frame is rendered or the terminal is restored.
    failure: Mutex<Option<io::Error>>,
}

impl DrainState {
    fn new() -> Self {
        Self {
            pending: AtomicUsize::new(0),
            failure: Mutex::new(None),
        }
    }

    fn drained(&self) -> bool {
        self.pending.load(Ordering::Acquire) == 0
    }

    fn take_failure(&self) -> Option<io::Error> {
        self.failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Hands frame and terminal-session bytes to a dedicated writer thread so a slow terminal cannot
/// block the owner loop: [`FrameWriter`] (the `CrosstermBackend` writer) and the terminal-session
/// entry and restoration sequences all enqueue onto the same ordered channel through a clone of
/// this handle, so their bytes are written in the order they were produced and never interleaved
/// or dropped. See `docs/architecture/background-tasks.md`'s "Terminal Output Writer" section for
/// the design and the alternatives considered.
#[derive(Clone)]
pub struct FrameSink {
    chunks: Sender<Vec<u8>>,
    state: Arc<DrainState>,
    /// A direct handle to the same destination the writer thread writes to, used only as a
    /// fallback when the ordered channel cannot be used: see `restore_commands`.
    direct: Arc<Mutex<dyn io::Write + Send>>,
}

impl FrameSink {
    /// Whether every byte handed to the writer thread so far has been written: the previously
    /// produced frame (or the entry sequence, before the first frame) has fully drained. The
    /// owner loop renders a new frame only once this is true, coalescing instead of blocking: a
    /// slow terminal paces frame production at its own drain rate rather than stalling scan
    /// ingestion or input handling.
    pub(crate) fn previous_frame_drained(&self) -> bool {
        self.state.drained()
    }

    /// Takes the first write error the background thread hit, if any.
    pub(crate) fn take_failure(&self) -> Option<io::Error> {
        self.state.take_failure()
    }

    /// Hands `bytes` to the writer thread, ordered after everything already queued. Returns
    /// immediately without waiting for the terminal to absorb anything, which is the point: see
    /// [`FrameWriter::flush`]. Empty input is a no-op.
    pub(crate) fn enqueue(&self, bytes: Vec<u8>) -> io::Result<()> {
        self.try_enqueue(bytes).map_err(|_bytes| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "the terminal writer thread has stopped",
            )
        })
    }

    /// Like `enqueue`, but returns `bytes` back on failure instead of discarding them, so a
    /// caller that cannot afford to lose them (restoration) can fall back to writing them
    /// another way. This fails once the writer thread has stopped entirely: its receiver was
    /// dropped, for example because it panicked and fully unwound.
    fn try_enqueue(&self, bytes: Vec<u8>) -> Result<(), Vec<u8>> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.state.pending.fetch_add(1, Ordering::Release);
        self.chunks.send(bytes).map_err(|error| {
            self.state.pending.fetch_sub(1, Ordering::Release);
            error.into_inner()
        })
    }

    /// Waits for the writer thread to confirm every byte handed to it so far has been written,
    /// bounded so a terminal that never drains cannot hang the caller. Returns whether it drained
    /// within `bound`.
    pub(crate) fn wait_drained(&self, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        while !self.state.drained() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(DRAIN_POLL_INTERVAL);
        }
        true
    }
}

/// `io::Write` handle the owner loop's `CrosstermBackend` writes frames through. Buffers one
/// flush's worth of bytes locally (no synchronization: only the owner loop thread ever touches
/// it) and hands the whole buffer to the shared [`FrameSink`] as one ordered chunk per `flush`,
/// which is where `ratatui::Terminal::draw` hands control back after a frame.
pub(crate) struct FrameWriter {
    pending: Vec<u8>,
    sink: FrameSink,
}

impl io::Write for FrameWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.pending.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        let bytes = std::mem::take(&mut self.pending);
        self.sink.enqueue(bytes)
    }
}

/// Spawns the dedicated terminal-output writer thread and returns the owner loop's `io::Write`
/// handle (for `CrosstermBackend`) and its [`FrameSink`] (for the render gate, and for ordering
/// terminal-session entry and restoration after it).
///
/// `direct` is a second handle to the same destination as `inner`, used only as a fallback when
/// the ordered channel and its writer thread cannot be used for restoration: see
/// `restore_commands`. It is never written to otherwise.
///
/// # Errors
/// Returns an I/O error if the thread cannot be spawned.
pub(crate) fn spawn_frame_writer(
    inner: impl io::Write + Send + 'static,
    direct: impl io::Write + Send + 'static,
) -> io::Result<(FrameWriter, FrameSink)> {
    let (chunks, receiver) = unbounded::<Vec<u8>>();
    let state = Arc::new(DrainState::new());
    let thread_state = Arc::clone(&state);
    thread::Builder::new()
        .name(FRAME_WRITER_THREAD_NAME.to_string())
        .spawn(move || run_frame_writer(inner, &receiver, &thread_state))?;
    let sink = FrameSink {
        chunks,
        state,
        direct: Arc::new(Mutex::new(direct)),
    };
    Ok((
        FrameWriter {
            pending: Vec::new(),
            sink: sink.clone(),
        },
        sink,
    ))
}

/// Body of the dedicated terminal-output writer thread: writes each queued chunk, in order, with
/// ordinary blocking I/O however slowly the terminal absorbs it. Decrements `pending` once for
/// the chunk just received, whether it was written successfully or discarded on the failure path
/// below, so the count always matches exactly how many handed-over chunks have not yet been
/// accounted for.
fn run_frame_writer(mut inner: impl io::Write, receiver: &Receiver<Vec<u8>>, state: &DrainState) {
    while let Ok(chunk) = receiver.recv() {
        let result = inner.write_all(&chunk).and_then(|()| inner.flush());
        if let Err(error) = result {
            let mut failure = state
                .failure
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if failure.is_none() {
                *failure = Some(error);
            }
            drop(failure);
            state.pending.fetch_sub(1, Ordering::Release);
            // This chunk's write failed, possibly transiently: drain whatever is already queued
            // without writing it (those are stale frames produced before the failure was known,
            // not worth delivering late), but keep trying later chunks individually rather than
            // giving up permanently - a transient error should not cost a later restoration
            // sequence its chance to be written. The owner loop surfaces the first recorded
            // failure at its next render or at exit.
            for _dropped in receiver.try_iter() {
                state.pending.fetch_sub(1, Ordering::Release);
            }
            continue;
        }
        state.pending.fetch_sub(1, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalState {
    #[default]
    Inactive,
    Raw,
    Active,
    Restored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalTransition {
    EnterRaw,
    EnterAlternate,
    Restore,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
#[error("invalid terminal transition {transition:?} from {state:?}")]
pub struct TerminalTransitionError {
    state: TerminalState,
    transition: TerminalTransition,
}

impl TerminalState {
    /// # Errors
    /// Returns a transition error when the requested lifecycle edge is invalid.
    pub fn transition(
        self,
        transition: TerminalTransition,
    ) -> Result<Self, TerminalTransitionError> {
        match (self, transition) {
            (Self::Inactive, TerminalTransition::EnterRaw) => Ok(Self::Raw),
            (Self::Raw, TerminalTransition::EnterAlternate) => Ok(Self::Active),
            (Self::Inactive | Self::Raw | Self::Active, TerminalTransition::Restore)
            | (Self::Restored, TerminalTransition::Restore) => Ok(Self::Restored),
            (state, transition) => Err(TerminalTransitionError { state, transition }),
        }
    }

    const fn has_raw_mode(self) -> bool {
        matches!(self, Self::Raw | Self::Active)
    }

    const fn has_alternate_screen(self) -> bool {
        matches!(self, Self::Active)
    }
}

/// # Errors
/// Returns a TTY error when stdin or stdout is not attached to a terminal.
pub fn validate_terminal() -> Result<(), AppError> {
    if !io::stdin().is_terminal() {
        return Err(AppError::Tty("standard input is not a TTY".to_string()));
    }
    if !io::stdout().is_terminal() {
        return Err(AppError::Tty("standard output is not a TTY".to_string()));
    }
    Ok(())
}

pub struct TerminalSession {
    state: TerminalState,
    active: Arc<AtomicBool>,
    mouse_capture: bool,
    frame_sink: FrameSink,
    previous_panic_hook: Arc<Mutex<Option<PanicHook>>>,
}

impl TerminalSession {
    /// # Errors
    /// Returns a terminal error if raw mode or alternate-screen entry fails.
    pub fn enter(frame_sink: FrameSink) -> Result<Self, AppError> {
        Self::enter_with_mouse(false, frame_sink)
    }

    /// # Errors
    /// Returns a terminal error if raw mode or alternate-screen entry fails.
    pub fn enter_with_mouse(mouse_capture: bool, frame_sink: FrameSink) -> Result<Self, AppError> {
        let mut session = Self {
            state: TerminalState::Inactive,
            active: Arc::new(AtomicBool::new(false)),
            mouse_capture,
            frame_sink,
            previous_panic_hook: Arc::new(Mutex::new(None)),
        };

        enable_raw_mode().map_err(|error| AppError::terminal("raw-mode entry", error))?;
        session.state = session
            .state
            .transition(TerminalTransition::EnterRaw)
            .map_err(|error| AppError::Invariant(error.to_string()))?;
        session.active.store(true, Ordering::Release);
        let enter_result = encode_enter_commands(mouse_capture)
            .and_then(|bytes| session.frame_sink.enqueue(bytes));
        if let Err(error) = enter_result {
            let _ = session.restore();
            return Err(AppError::terminal("alternate-screen entry", error));
        }
        session.state = session
            .state
            .transition(TerminalTransition::EnterAlternate)
            .map_err(|error| AppError::Invariant(error.to_string()))?;
        session.install_panic_hook();
        Ok(session)
    }

    /// # Errors
    /// Returns a terminal error if any explicit restoration operation fails. A terminal that
    /// never drains the restoration sequence within a bounded wait does not fail this call: see
    /// `restore_commands`.
    pub fn restore(&mut self) -> Result<(), AppError> {
        if self.state == TerminalState::Restored {
            return Ok(());
        }

        let was_active = self.active.swap(false, Ordering::AcqRel);
        let terminal_result = if was_active && self.state.has_alternate_screen() {
            restore_commands(&self.frame_sink, self.mouse_capture)
                .map_err(|error| AppError::terminal("restoration", error))
        } else {
            Ok(())
        };
        let raw_result = if was_active && self.state.has_raw_mode() {
            disable_raw_mode().map_err(|error| AppError::terminal("raw-mode restoration", error))
        } else {
            Ok(())
        };
        self.state = self
            .state
            .transition(TerminalTransition::Restore)
            .map_err(|error| AppError::Invariant(error.to_string()))?;
        self.restore_panic_hook();

        terminal_result.and(raw_result)
    }

    fn install_panic_hook(&mut self) {
        let previous = panic::take_hook();
        *self
            .previous_panic_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(previous);
        let active = self.active.clone();
        let previous = self.previous_panic_hook.clone();
        let mouse_capture = self.mouse_capture;
        let frame_sink = self.frame_sink.clone();
        panic::set_hook(Box::new(move |info| {
            if active.swap(false, Ordering::AcqRel) {
                let _ = restore_commands(&frame_sink, mouse_capture);
                let _ = disable_raw_mode();
            }
            if let Some(previous) = previous
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                previous(info);
            }
        }));
    }

    fn restore_panic_hook(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let _installed = panic::take_hook();
        if let Some(previous) = self
            .previous_panic_hook
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            panic::set_hook(previous);
        }
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Encodes the terminal-restoration sequence (reset colours, show the cursor, leave the
/// alternate screen and, when requested, disable mouse capture) as bytes, without writing them
/// anywhere: shared by the ordered (writer-thread) and the direct-write fallback paths below so
/// both produce byte-identical output.
fn encode_restore_commands(mouse_capture: bool) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    if mouse_capture {
        queue!(
            buffer,
            ResetColor,
            Show,
            DisableMouseCapture,
            EnableLineWrap,
            LeaveAlternateScreen
        )?;
    } else {
        queue!(
            buffer,
            ResetColor,
            Show,
            EnableLineWrap,
            LeaveAlternateScreen
        )?;
    }
    Ok(buffer)
}

/// Whether the current thread is the dedicated terminal-output writer thread. True only while
/// the global panic hook is running because that thread itself panicked: nothing else ever runs
/// on it.
fn is_frame_writer_thread() -> bool {
    thread::current().name() == Some(FRAME_WRITER_THREAD_NAME)
}

/// Writes `buffer` straight to the real terminal, bypassing the writer thread and its ordered
/// channel entirely. See `restore_commands` for when this is used instead of the ordered path.
fn write_restore_directly(sink: &FrameSink, buffer: &[u8]) -> io::Result<()> {
    let mut direct = sink
        .direct
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    direct.write_all(buffer)?;
    direct.flush()
}

/// Restores the terminal (reset colours, show the cursor, leave the alternate screen, and
/// re-enable line wrap and mouse reporting as needed). Ordered after every frame byte produced so
/// far by handing the sequence to the same writer thread and channel frames go through, so it can
/// never be written before or interleaved with them, including when the program exits during a
/// slow drain.
///
/// Two situations cannot use that ordered path, because there is nobody left able to drain it, so
/// this writes directly instead:
/// - this function is itself running on the writer thread: that only happens when the writer
///   thread panics and the global panic hook runs on it, and waiting for that very thread to
///   drain a chunk it just handed to itself would simply time out, since nothing else ever reads
///   from its channel;
/// - the writer thread has already stopped entirely (its receiver was dropped, for example
///   because an earlier panic already unwound it): enqueuing then fails, and the bytes are handed
///   back instead of being written.
fn restore_commands(sink: &FrameSink, mouse_capture: bool) -> io::Result<()> {
    let buffer = encode_restore_commands(mouse_capture)?;
    if is_frame_writer_thread() {
        return write_restore_directly(sink, &buffer);
    }
    match sink.try_enqueue(buffer) {
        Ok(()) => {
            sink.wait_drained(RESTORE_DRAIN_BOUND);
            if let Some(error) = sink.take_failure() {
                return Err(error);
            }
            Ok(())
        }
        Err(bytes) => write_restore_directly(sink, &bytes),
    }
}

/// Encodes the terminal-session entry sequence (enter the alternate screen, disable line wrap
/// and, when requested, capture mouse events) as bytes, without writing them anywhere: the caller
/// hands them to a `FrameSink` so they are ordered with whatever else writes through it.
fn encode_enter_commands(mouse_capture: bool) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    if mouse_capture {
        queue!(
            buffer,
            EnterAlternateScreen,
            EnableMouseCapture,
            DisableLineWrap
        )?;
    } else {
        queue!(buffer, EnterAlternateScreen, DisableLineWrap)?;
    }
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use super::{
        DrainState, FrameSink, SplitColorWriter, encode_restore_commands, restore_commands,
        spawn_frame_writer, unbounded,
    };

    #[test]
    fn split_color_writer_keeps_truecolour_commands_separate_across_writes() {
        let mut writer = SplitColorWriter::new(Vec::new());
        writer
            .write_all(b"\x1b[38;2;48;192;149;48")
            .expect("first ANSI fragment should write");
        writer
            .write_all(b";2;30;30;46mX")
            .expect("second ANSI fragment should write");
        writer.flush().expect("ANSI output should flush");

        assert_eq!(writer.inner, b"\x1b[38;2;48;192;149m\x1b[48;2;30;30;46mX");
    }

    /// A writer that blocks its first `write_all` until released, so a test can make a "frame"
    /// still be draining when restoration is requested, then inspect everything actually
    /// written, in order.
    struct BlockFirstWrite {
        release: mpsc::Receiver<()>,
        blocked_once: bool,
        log: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for BlockFirstWrite {
        fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
            if !self.blocked_once {
                self.blocked_once = true;
                let _ = self.release.recv();
            }
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buffer);
            Ok(())
        }

        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.write_all(buffer).map(|()| buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn restoration_follows_every_frame_byte_and_a_hung_writer_times_out() {
        let log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (release_tx, release_rx) = mpsc::channel();
        let writer = BlockFirstWrite {
            release: release_rx,
            blocked_once: false,
            log: Arc::clone(&log),
        };
        let (mut frame_writer, sink) =
            spawn_frame_writer(writer, Vec::new()).expect("the writer thread should spawn");
        assert!(
            sink.previous_frame_drained(),
            "a fresh writer has nothing in flight"
        );

        // The owner loop's `CrosstermBackend` writes a frame, then flushes it off: this is what
        // hands bytes to the writer thread, which is now blocked inside `write_all` on them.
        frame_writer
            .write_all(b"FRAME")
            .expect("buffering a frame never blocks");
        frame_writer
            .flush()
            .expect("handing a frame off never blocks");

        // The gate is closed while that frame is still draining, and a bounded wait gives up
        // rather than hanging forever: a hung terminal cannot hang exit.
        assert!(!sink.previous_frame_drained());
        assert!(!sink.wait_drained(Duration::from_millis(50)));

        // Restoration is requested while the frame is still draining (exiting during a slow
        // drain). It must queue behind the frame's bytes, never race ahead of them.
        sink.enqueue(b"RESTORE".to_vec())
            .expect("enqueuing restoration never blocks");

        release_tx
            .send(())
            .expect("the writer thread is waiting to be released");
        assert!(
            sink.wait_drained(Duration::from_secs(5)),
            "the writer thread should catch up once released"
        );

        let written = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            written, b"FRAMERESTORE",
            "no frame bytes after the restore sequence"
        );
    }

    /// A writer that appends everything written to a shared log, so a test can inspect it after
    /// the writer value itself has been moved into `spawn_frame_writer`.
    #[derive(Clone)]
    struct LoggingWriter {
        log: Arc<Mutex<Vec<u8>>>,
    }

    impl std::io::Write for LoggingWriter {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn pending_count_tracks_multiple_queued_chunks_against_a_blocked_writer() {
        let log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let (release_tx, release_rx) = mpsc::channel();
        let writer = BlockFirstWrite {
            release: release_rx,
            blocked_once: false,
            log: Arc::clone(&log),
        };
        let (mut frame_writer, sink) =
            spawn_frame_writer(writer, Vec::new()).expect("the writer thread should spawn");

        // The first flush hands a chunk to the writer thread, which immediately blocks inside
        // `write_all` on it (see `BlockFirstWrite`) and so never reaches its next `recv`.
        frame_writer
            .write_all(b"A")
            .expect("buffering never blocks");
        frame_writer
            .flush()
            .expect("handing a chunk off never blocks");

        // Two more chunks are queued while the writer thread is still stuck on the first: the
        // count must track all three, not just whichever the writer thread last touched.
        sink.enqueue(b"B".to_vec()).expect("enqueuing never blocks");
        sink.enqueue(b"C".to_vec()).expect("enqueuing never blocks");
        assert!(
            !sink.previous_frame_drained(),
            "three chunks are queued and at most one has even started writing"
        );

        release_tx
            .send(())
            .expect("the writer thread is waiting to be released");
        assert!(
            sink.wait_drained(Duration::from_secs(5)),
            "the writer thread should catch up on all three chunks once released"
        );

        let written = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            written, b"ABC",
            "every queued chunk should be written, in order"
        );
    }

    #[test]
    fn restore_falls_back_to_a_direct_write_once_the_writer_thread_has_stopped() {
        // An unbounded channel whose receiver is already dropped simulates the writer thread
        // having already exited entirely (for example, it panicked and fully unwound): nothing
        // will ever read from `chunks` again, so enqueuing onto it must fail.
        let (chunks, receiver) = unbounded::<Vec<u8>>();
        drop(receiver);
        let direct_log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = FrameSink {
            chunks,
            state: Arc::new(DrainState::new()),
            direct: Arc::new(Mutex::new(LoggingWriter {
                log: Arc::clone(&direct_log),
            })),
        };

        restore_commands(&sink, false).expect("falling back to a direct write should not fail");

        let expected =
            encode_restore_commands(false).expect("encoding the restore sequence should not fail");
        assert_eq!(
            *direct_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            expected,
            "restore_commands must write directly once the writer thread has stopped"
        );
    }

    /// A writer that, when it receives a specific marker chunk, calls `restore_commands` on its
    /// own thread (exactly as the installed panic hook does when a panic happens on the writer
    /// thread, since the hook runs on whichever thread panicked) before actually panicking, so a
    /// test can drive the real writer thread into that situation and confirm restoration still
    /// reaches the terminal without hanging.
    struct PanicAfterRestoringOnMarker {
        marker: &'static [u8],
        sink: Arc<Mutex<Option<FrameSink>>>,
        restored: mpsc::Sender<std::io::Result<()>>,
    }

    impl std::io::Write for PanicAfterRestoringOnMarker {
        fn write_all(&mut self, buffer: &[u8]) -> std::io::Result<()> {
            if buffer == self.marker {
                let sink = self
                    .sink
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .expect("the sink is installed before the marker chunk is sent");
                let result = restore_commands(&sink, false)
                    .map_err(|error| std::io::Error::new(error.kind(), error.to_string()));
                let _ = self.restored.send(result);
                panic!("simulated panic on the terminal writer thread");
            }
            Ok(())
        }

        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.write_all(buffer).map(|()| buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_panic_on_the_writer_thread_still_restores_the_terminal_directly() {
        let direct_log: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_cell: Arc<Mutex<Option<FrameSink>>> = Arc::new(Mutex::new(None));
        let (restored_tx, restored_rx) = mpsc::channel();
        let writer = PanicAfterRestoringOnMarker {
            marker: b"PANIC",
            sink: Arc::clone(&sink_cell),
            restored: restored_tx,
        };
        let direct = LoggingWriter {
            log: Arc::clone(&direct_log),
        };
        let (_frame_writer, sink) =
            spawn_frame_writer(writer, direct).expect("the writer thread should spawn");
        *sink_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(sink.clone());

        sink.enqueue(b"PANIC".to_vec())
            .expect("enqueuing never blocks");

        let restored_result = restored_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the writer thread should call restore_commands before it panics");
        restored_result.expect("restoring from the writer thread itself should not fail");

        let expected =
            encode_restore_commands(false).expect("encoding the restore sequence should not fail");
        assert_eq!(
            *direct_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            expected,
            "the restore bytes must still reach the terminal when the writer thread itself panics"
        );
    }
}
