//! A running program and the protocols that drive it, shared by everything that drives one.
//!
//! [`Live`] is a program under observation: its pseudo-terminal session, its event log, and the
//! accounting that tells when the screen reflects the input sent. [`Drive`] is what a driver of a
//! `Live` implements (the scenario executor, and the interactive supervisor of `crate::tui`); its
//! provided methods are the protocols every driver shares, so the scenario `delete` step and
//! `cargo xtask tui delete` run one implementation: waiting, settling, selecting an entry, the
//! deletion protocol, and asking to quit.
//!
//! # Waiting
//!
//! Every wait has a deadline and ends in one of three ways: the condition held, the deadline
//! passed, or the program ended (its exit seen and all its output read) without the condition
//! holding. The last case fails at once instead of running the clock out. Waits never sleep and
//! never look at output bytes: they drive the session and the event log and evaluate a condition
//! after every bit of progress.
//!
//! # Inputs and frames
//!
//! The event channel's `frame` event carries `inputs`, the number of terminal input events the
//! program had consumed when it drew that frame. A [`Live`] counts every input event it writes,
//! but the program may have counted some of its own before the first: `excise` on Windows counts
//! one before any key is sent. So the live program takes the `inputs` of the latest frame, or 0
//! before the first, when it writes the first input, and calls it the input baseline. A frame
//! *reflects* the inputs sent so far when its counter is at least the baseline plus their number
//! and it was observed after the last input was written. An input that the program counts of its
//! own only after the first one was written is not accounted for. Waiting for such a frame is what
//! `settle` means, and with the marks below it is how a driver knows that the screen it is about
//! to read shows the effect of a key and not the moment before it. That is evidence for what
//! decides nothing destructive, and no more: it takes one write for one input event, which a
//! terminal does not promise (see "The input barrier").
//!
//! # Frame marks
//!
//! A `frame` event says that the program drew, not that the drawing has reached the screen model.
//! The program writes the event when it *queues* the frame for its terminal writer, and the
//! terminal can hold the bytes back for as long as it likes: a thread that is not scheduled, a
//! slow terminal, a pseudo-terminal whose reader is paced. A program that opens the event channel
//! therefore also follows every frame with a mark in its terminal output, and says so in its
//! `hello` (`frame_marks`). The session reads the marks out of the stream
//! ([`PtySession::frame_shown`]).
//!
//! On a Unix pseudo-terminal the mark follows its frame's bytes, so the screen model shows a frame
//! exactly when the session has read its mark: it has then been through every byte of that frame,
//! and of every frame before it. That screen is *exact* ([`SCREEN_IS_EXACT`]). `ConPTY` does not
//! relay the output: it parses it into a buffer of its own, paints that buffer on its own timer,
//! and passes the mark on as soon as it has parsed it, so there each mark arrives *before* the
//! paint of its frame. The mark says that the console host has the frame, and the screen model
//! shows it from the host's next paint: the first output that is not a mark, read after the mark,
//! shows that frame or a later one. A step on such a terminal that decides nothing destructive
//! therefore waits for that paint, and then for the bounded quiet read, or for
//! [`Live::frame_window`] to pass after the mark with nothing painted, which means that nothing
//! needed painting ([`Live::screen_shows`]). Where that window is zero, as on Unix, the mark alone
//! is exact.
//!
//! No rule proves a `ConPTY` paint complete, so none of them makes that screen exact. A repaint can
//! be split after a cursor or control prefix, and a paint begun before the mark can be flushed
//! after it, so that the screen shows a stale dialog however quiet the output has been and however
//! long ago the mark was read. The harness therefore *confirms no deletion from such a screen*:
//! what could, which is [`Drive::confirm_deletion`] (the scenario `delete` step with a dialog and
//! `cargo xtask tui delete`), the scenario `delete` step with no dialog, and the driver's guard of
//! `keys`, refuses before it sends any key, a Backspace included ([`Live::deletion_refusal`]),
//! and the pseudo-terminal runner skips the scenarios that delete (`e2e::select`). Deletions are
//! exercised on such a platform by the in-process runner, which has no terminal between the
//! program and the screen it reads. The reads that a deletion depends on stay as a second line of
//! defence behind that refusal: the dialog read after the Backspace of
//! [`Drive::confirm_deletion`] and, in the mode that has no dialog, the read of the selected-item
//! panel before the Backspace and the check after it that no dialog opened wait for
//! [`Live::screen_surely_shows`], which is the mark on an exact screen and never true on one that
//! is not, so they could not be satisfied there.
//!
//! What the screen shows is then exact up to the frame whose mark has been read. Which frame a
//! read must wait for is the question of the next section. A program that does not mark its frames
//! gives no such evidence: what decides nothing destructive keeps the bounded read of
//! [`Drive::catch_up`], and what could confirm a deletion refuses such a program before it sends a
//! key ([`Drive::confirm_deletion`]).
//!
//! # The input barrier
//!
//! A frame that counts the inputs sent ([`Live::reflecting_frame_seq`]) cannot say that the program
//! has read them. It takes one write for one input event, and a terminal promises nothing of the
//! kind: `ESC DEL` is Alt+Backspace when the program reads both bytes at once and Esc then
//! Backspace when it does not, an `ESC` followed at once by `ESC [ A` is Esc, `[`, `A`, and a
//! resize is not a write at all. The program counts the events it really read, so a frame can
//! count the inputs sent while events they became are still unread, and a key that could confirm
//! a deletion must not be sent on that.
//!
//! What a deletion depends on is exact instead. A program that says so in its `hello`
//! (`input_barrier`) answers a *barrier request*, the byte [`BARRIER`] written to its input: it
//! reads it in order with the rest, and draws a frame whose `barriers` counts the requests it has
//! read. The first frame whose `barriers` reaches the number of requests written answers the last
//! of them, and since the program reads its input in order, everything written before that request
//! has been read, and the frame shows what it did, however the terminal cut the bytes into events
//! ([`Drive::barrier`]). Once the screen shows that frame (its mark, as above) it shows every
//! dialog there is: a dialog opens on an input and never by itself, and nothing is queued behind
//! the request that the harness does not know of. Every read that a deletion depends on is made
//! after a barrier, and the key that confirms is written after it, so nothing the harness wrote
//! can reach the program between what it read and that key.
//!
//! At most one request is outstanding: one that was never answered (a program that is not
//! reading) is waited for before another is written, so that the answer to an old request cannot
//! stand for a new one. The harness is the only writer of the byte (`ctrl+]` has no encoding as a
//! key), so its count of requests is the program's.
//!
//! What the barrier orders is the terminal input. A resize reaches the program as a signal, not as
//! a byte, so it is ordered with nothing: the `resize` step waits for the frame that counts it
//! before it returns, and the driver has no resize.
//!
//! A program that does not say `input_barrier` (a build from before it) is sent no key that could
//! confirm a deletion ([`Live::deletion_refusal`]). The count above and the bounded read of
//! [`Drive::catch_up`] stay for what decides nothing destructive.
//!
//! # Raw deletion requests
//!
//! The protocols keep a confirmation from meeting a dialog nobody verified because they send the
//! keys that ask for a deletion and the keys that confirm one. A scenario's own `key` and `type`
//! steps are not bound by them. A raw Backspace asks for a deletion dialog that no protocol
//! verifies ([`crate::pty::input`] says which bytes the program reads that way, `alt+backspace`
//! among them), and so do bytes that only begin to: the program's parser joins the bytes of an
//! escape sequence across writes, so `alt+[` and the text `121u` written after it are
//! `ESC [ 121 u`, which the program reads as `y` with no `y` in any write. That dialog can be
//! unread, covered by a prompt, or still to be drawn when a later write that could confirm it goes
//! out: a raw `y` or Enter, the filter text and the Enter of `select`, the `y` of `quit`.
//!
//! [`Live`] scans every byte it writes ([`crate::pty::input::InputScan`]): a step's own, the
//! protocols' (Backspace, `/`, a filter's text, Enter, Esc, `q`, `y`), and the barrier request, so
//! that what the scan holds between writes is what the program's parser holds. For each write it
//! says whether the bytes so far can be read as a request for a deletion and whether they can be
//! read as a confirmation ([`Live::reading_of`]): a sequence that one write begins and a later
//! write continues is both, whatever its bytes. [`Live::note_raw_write`] asks it, before a step of
//! the scenario's own writes, about that write. Until one can be read as a request, nothing
//! changes: every fast path stays, and `quit` presses `y` on the frame event that counts its `q`.
//! From then on the run is engaged to its end, because a pending confirmation can hide behind the
//! quit prompt and one rule is simpler than a rule for when it is over. Every write that can be
//! read as a confirmation then goes through [`Drive::send_confirming`]: it writes a barrier, waits
//! for the answering frame and its mark, and sends the bytes only if that exact screen shows no
//! deletion dialog and what the sender needs (the filter prompt for `select`, the plain quit
//! prompt for `quit`, the one that lists no waiting check, so none is pending beneath it).
//! Otherwise the step fails with [`FailureCause::DeleteRefused`] and nothing is sent. Two writes
//! are refused outright, because no barrier can come between what they hold and what the program
//! may read them with: the escape byte and a confirmation in one write (`alt+y`), and a write that
//! continues an escape sequence that an earlier write began and nothing has finished, since a
//! barrier written behind `ESC [` joins the sequence and is never answered
//! ([`Live::holds_an_escape_sequence`]). Where the screen is not exact, or the program does not
//! mark its frames or answer a barrier, every such write is refused, as a deletion is
//! ([`Live::deletion_refusal`]), and the pseudo-terminal runner does not run a scenario that asks
//! for a deletion that way where the screen is not exact (see `plan::raw_deletion_request`, which
//! reads the steps' own writes the same way). The interactive driver is not engaged: its `keys`
//! asks the scan about every input, whatever the earlier keys were, and guards the ones that can
//! confirm.
//!
//! # Catching up with a frame
//!
//! A driver that ends on a frame event (`settle`, `delete`, `resize`) waits until the screen
//! shows that frame before it returns ([`Drive::catch_up`]), so that what comes after can read the
//! screen once. For a program that does not mark its frames it reads the terminal for a while
//! instead, and how long depends on the terminal: see [`FRAME_WINDOW`]. For one that does, on a
//! terminal that paints on its own timer, it reads on until the paint that follows the mark has
//! come, and the output after it has been quiet for a moment.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use crate::{
    events::{Event, EventLog, Payload},
    pty::{
        BoxView, PtyError, PtySession, Screen,
        input::{InputScan, Reading, is_escape_then_confirmation},
        keys::BARRIER,
        ui::{
            DeleteDialog, DialogView, FilterPrompt, Inspector, deletion_dialog_visible,
            dialog_view, filter_prompt, inspector, offers_plain_quit,
        },
    },
    safety::FixtureRoot,
    scenario::{ConfirmKey, EntryKind},
};

use super::{
    delete::{Verified, verify},
    outcome::{FailureCause, RunError},
};

/// How long a wait blocks for terminal output before it looks at everything else again.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How long `ConPTY` can hold a frame's output back after the program has reported the frame.
///
/// `ConPTY` is a renderer, not a pipe: it keeps its own copy of the screen and sends what changed
/// in paints that are typically 16 ms apart, so a frame that is drawn soon after a paint waits for
/// the next one. In the recordings of failed runs on GitHub-hosted Windows runners, 30 isolated
/// frames reached the harness 4 to 22 ms after the program reported them (median 12.5 ms), and the
/// gaps between 188 paints of a console that was being redrawn had a median of 15.6 ms and a 99th
/// percentile of 23 ms; the longest, 54 ms, came while the program was starting. These are upper
/// bounds, because the program's clock and the recording's differ by an offset that only the
/// moments the keys were sent bound. 100 ms is more than four times the longest delay once the
/// program was running, and about twice the longest of all.
pub(crate) const CONPTY_FRAME_WINDOW: Duration = Duration::from_millis(100);
/// Whether the pseudo-terminal of this platform ties its screen to a frame exactly: a screen that
/// has read the mark of a frame shows that frame, with nothing left to wait for. True on Unix,
/// where the mark follows its frame's bytes. False on Windows: `ConPTY` passes a mark on when it
/// has parsed the frame, paints on its own timer, and no rule proves a paint complete, so its
/// screen can show a dialog that is gone (see "Frame marks" above). Where it is false the harness
/// confirms no deletion from the screen ([`Live::deletion_refusal`]) and the pseudo-terminal runner
/// skips the scenarios that delete.
pub(crate) const SCREEN_IS_EXACT: bool = !cfg!(windows);
/// How long a step that waited for a frame event keeps reading the terminal before it trusts the
/// screen, when the program does not mark its frames: `ConPTY`'s window on Windows, none
/// elsewhere. The program hands a frame to its writer thread before it reports the frame, and a
/// Unix pseudo-terminal passes bytes on as they are written, so the frame is at most a thread
/// switch behind its event, which the bounded tail in [`Drive::catch_up`] covers. A program that
/// marks its frames needs no window on Unix, where its mark follows its frame. On Windows the
/// window is the longest the screen is given after the mark to show the frame, which `ConPTY`
/// paints later (see "Frame marks" above): enough for a read that decides nothing destructive, and
/// no proof for one that does.
pub(crate) const FRAME_WINDOW: Duration = if SCREEN_IS_EXACT {
    Duration::ZERO
} else {
    CONPTY_FRAME_WINDOW
};
/// After the frame window ([`Drive::catch_up`]), output that is still arriving is read until it
/// has been quiet this long...
const SETTLE_QUIET: Duration = Duration::from_millis(3);
/// ...or this long has passed.
const SETTLE_LIMIT: Duration = Duration::from_millis(20);

/// Why a deletion is refused when the program does not mark its frames in the terminal output:
/// nothing then says when the screen shows the dialog the program has, so a confirmation could
/// reach a dialog that was never verified.
const NO_FRAME_MARKS: &str = "the program does not mark its frames in the terminal \
     output (its `hello` has no `frame_marks`), so nothing says when the screen shows the \
     dialog the program has, and a confirmation key could reach a deletion dialog that was \
     never verified; no key was sent. Run a build that marks its frames";

/// Why a deletion is refused when the program does not answer input barrier requests: nothing
/// then says that it has read what was written to it, however the terminal cut the bytes into
/// events, so a confirmation could reach a dialog that a key still unread opens.
const NO_INPUT_BARRIER: &str = "the program does not answer input barrier requests (its `hello` \
     has no `input_barrier`), so nothing says that it has read what was written to it, and a \
     confirmation key could reach a deletion dialog that was never verified; no key was sent. \
     Run a build that answers them";

/// Why no deletion is confirmed from the screen of a terminal that is not exact
/// ([`SCREEN_IS_EXACT`]): where it paints on its own timer, nothing proves that a screen shows the
/// frame whose mark was read, and a dialog that is gone, or another entry's, could be read and
/// confirmed.
pub(crate) const SCREEN_NOT_EXACT: &str = "the terminal repaints the screen on its own timer \
     (Windows' console host), so the harness cannot tie the screen to a frame and confirms no \
     deletion from it";

/// The bytes of a Backspace key press.
pub(crate) const BACKSPACE: [u8; 1] = [0x7f];
/// The bytes of an Enter key press.
pub(crate) const ENTER: [u8; 1] = *b"\r";

/// How a bounded wait ended.
#[derive(Debug)]
pub(crate) enum Waited<T> {
    /// The condition held.
    Ready(T),
    /// The deadline passed.
    TimedOut,
    /// The program ended first.
    Exited,
}

impl<T> Waited<T> {
    /// The outcome without its value.
    pub(crate) const fn discard(&self) -> Waited<()> {
        match self {
            Self::Ready(_) => Waited::Ready(()),
            Self::TimedOut => Waited::TimedOut,
            Self::Exited => Waited::Exited,
        }
    }
}

/// An input event that was just written to the program.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sent {
    /// The instant just before the write.
    pub(crate) at: Instant,
    /// How many inputs have been sent, this one included.
    pub(crate) number: u64,
    /// The `inputs` counter at which a frame reflects this input.
    pub(crate) reflecting: u64,
    /// Whether the latest frame reflected every earlier input when this one was written, so the
    /// frame that answers this input is not one an earlier input was already waiting for.
    pub(crate) isolated: bool,
}

/// What a program's `hello` says it does, which is what decides whether a deletion may be
/// confirmed from what it shows (see "Frame marks" and "The input barrier" in the module
/// documentation).
#[derive(Debug, Clone, Copy)]
struct Capabilities {
    /// Whether the program marks its frames in the terminal output (`frame_marks`).
    frame_marks: bool,
    /// Whether the program answers input barrier requests (`input_barrier`).
    input_barrier: bool,
}

/// A program under observation. See the module documentation.
pub(crate) struct Live {
    pub(crate) session: PtySession,
    pub(crate) events: EventLog,
    /// Terminal input events written so far.
    inputs_sent: u64,
    last_input_at: Option<Instant>,
    /// The `inputs` the program had counted when the first input was written: inputs of its own,
    /// which the driver did not send. `None` until that moment. A frame that reflects the inputs
    /// sent counts them on top of this baseline.
    input_baseline: Option<u64>,
    /// How long [`Drive::catch_up`] reads the terminal after a frame event, before its bounded
    /// tail, for a program that does not mark its frames. For one that does, a window that is not
    /// zero says that the terminal paints on its own timer (`ConPTY`): it is the longest the screen
    /// is given after the mark to show the frame when nothing is painted (see "Frame marks"). A
    /// field so that a test can give a Unix terminal the delay `ConPTY` has.
    pub(crate) frame_window: Duration,
    /// What the `hello` said the program does. `None` until the `hello` has been read, and then
    /// the program is taken to do neither.
    capabilities: Option<Capabilities>,
    /// Input barrier requests written so far.
    barriers_sent: u64,
    /// What the program can read of every byte written to it so far: a step's own, the
    /// protocols', and the barrier request (see "Raw deletion requests" in the module
    /// documentation).
    input_scan: InputScan,
    /// Whether a write that a scenario made on its own could be read as a request for a deletion.
    /// Once it is true it stays true: the run is engaged to its end.
    raw_deletion_requested: bool,
}

impl Live {
    /// A program in `session` whose event channel is the file `events`.
    pub(crate) fn new(session: PtySession, events: PathBuf) -> Self {
        Self {
            session,
            events: EventLog::new(events),
            inputs_sent: 0,
            last_input_at: None,
            input_baseline: None,
            frame_window: FRAME_WINDOW,
            capabilities: None,
            barriers_sent: 0,
            input_scan: InputScan::default(),
            raw_deletion_requested: false,
        }
    }

    /// Reads what the event channel has appended, and checks the `hello` of the first line.
    pub(crate) fn poll_events(&mut self) -> Result<(), RunError> {
        if self.events.poll(Instant::now())? > 0 && self.capabilities.is_none() {
            self.check_hello()?;
        }
        Ok(())
    }

    /// Reads all pending terminal output and events.
    // Only the interactive driver calls this, and the driver builds on Unix only.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn pump(&mut self) -> Result<(), RunError> {
        self.session.pump()?;
        self.poll_events()
    }

    /// The first event must be the `hello` of the process that was started. An event file that
    /// belongs to another process would make every wait meaningless. It also says whether the
    /// program marks its frames.
    fn check_hello(&mut self) -> Result<(), RunError> {
        let Some(first) = self.events.events().first() else {
            return Ok(());
        };
        match &first.payload {
            Payload::Hello {
                pid,
                frame_marks,
                input_barrier,
                ..
            } if *pid == u64::from(self.session.pid()) => {
                self.capabilities = Some(Capabilities {
                    frame_marks: *frame_marks,
                    input_barrier: *input_barrier,
                });
                Ok(())
            }
            Payload::Hello { pid, .. } => Err(RunError::Protocol(format!(
                "the event channel says it belongs to process {pid}, but the process started \
                 was {}",
                self.session.pid()
            ))),
            other => Err(RunError::Protocol(format!(
                "the event channel starts with `{}` instead of `hello`",
                other.kind_name()
            ))),
        }
    }

    /// Writes one terminal input event.
    pub(crate) fn send_input(&mut self, bytes: &[u8]) -> Result<Sent, PtyError> {
        let isolated = self.frame_reflecting_inputs().is_some();
        self.take_input_baseline();
        self.inputs_sent += 1;
        // Before the write, so that bytes that may have reached the program count even if the
        // write then fails.
        self.input_scan.note(bytes);
        let at = self.session.send(bytes)?;
        self.last_input_at = Some(at);
        Ok(Sent {
            at,
            number: self.inputs_sent,
            reflecting: self.reflecting_counter(),
            isolated,
        })
    }

    /// What the program can read of `bytes` if they are written next, given every byte written so
    /// far (see [`InputScan::peek`]). Nothing changes.
    pub(crate) fn reading_of(&self, bytes: &[u8]) -> Reading {
        self.input_scan.peek(bytes)
    }

    /// Whether an escape sequence that an earlier write began is open, so that the next write
    /// continues it and no barrier can be written behind it (see
    /// [`InputScan::holds_a_sequence`]).
    pub(crate) const fn holds_an_escape_sequence(&self) -> bool {
        self.input_scan.holds_a_sequence()
    }

    /// Notes bytes that a scenario is about to write on its own, in a `key` or `type` step and
    /// outside every protocol, and engages the run if the program can read them as a request for a
    /// deletion (see "Raw deletion requests" in the module documentation). It is called before the
    /// write, so that bytes that may have reached the program count even if the write is then
    /// refused or fails. The bytes are not scanned here: [`Live::send_input`] does that, for every
    /// write.
    pub(crate) fn note_raw_write(&mut self, bytes: &[u8]) {
        if self.reading_of(bytes).request {
            self.raw_deletion_requested = true;
        }
    }

    /// Whether a write that a scenario made on its own could be read as a request for a deletion:
    /// the run is engaged, and every write that could confirm a deletion is checked first
    /// ([`Drive::send_confirming`]).
    pub(crate) const fn raw_deletion_requested(&self) -> bool {
        self.raw_deletion_requested
    }

    /// Resizes the terminal. The program counts the resize as an input event, so the frame that
    /// answers it carries it in its `inputs`.
    pub(crate) fn resize(&mut self, cols: u16, rows: u16) -> Result<Instant, PtyError> {
        self.take_input_baseline();
        let at = self.session.resize(cols, rows)?;
        self.inputs_sent += 1;
        self.last_input_at = Some(at);
        Ok(at)
    }

    /// Writes one input barrier request (see "The input barrier" in the module documentation).
    ///
    /// It is not an input of the driver: it is not counted in [`Live::inputs_sent`] and the
    /// program does not count it in the `inputs` of its frames. The count of requests is the one
    /// thing it changes: [`Live::barrier_answer_seq`] waits for a frame that answers it.
    pub(crate) fn send_barrier(&mut self) -> Result<Instant, PtyError> {
        self.barriers_sent += 1;
        self.input_scan.note(&[BARRIER]);
        self.session.send(&[BARRIER])
    }

    /// Whether the program answers input barrier requests, as its `hello` said. `false` for a
    /// program from before the barrier, and until the `hello` has been read.
    pub(crate) const fn input_barrier(&self) -> bool {
        matches!(
            self.capabilities,
            Some(Capabilities {
                input_barrier: true,
                ..
            })
        )
    }

    /// How many requests the latest frame answers: its `barriers` counter, or 0 before the first
    /// frame.
    fn latest_frame_barriers(&self) -> u64 {
        self.events
            .events()
            .iter()
            .rev()
            .find_map(|event| match event.payload {
                Payload::Frame { barriers, .. } => Some(barriers),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// Whether a request was written that no frame answers yet.
    pub(crate) fn barrier_outstanding(&self) -> bool {
        self.latest_frame_barriers() < self.barriers_sent
    }

    /// The `seq` of the first frame that answers the latest request written, if the program has
    /// drawn one: the first whose `barriers` counter has reached the number of requests written.
    pub(crate) fn barrier_answer_seq(&self) -> Option<u64> {
        if self.barriers_sent == 0 {
            return None;
        }
        self.events
            .events()
            .iter()
            .find_map(|event| match event.payload {
                Payload::Frame { seq, barriers, .. } if barriers >= self.barriers_sent => Some(seq),
                _ => None,
            })
    }

    /// Believes the program's frame over the driver's own count of inputs sent: two resizes in a
    /// row can reach the program as one.
    pub(crate) fn believe_latest_frame(&mut self) {
        self.inputs_sent = self.inputs_sent.max(self.latest_frame_inputs_sent());
    }

    /// Fixes the input baseline when the first input is about to be written, and leaves it alone
    /// afterwards: the `inputs` of the latest frame, or 0 before the first frame. A program may
    /// count inputs of its own before it is sent any, and a counter that includes them must not be
    /// taken for one that includes the first key.
    fn take_input_baseline(&mut self) {
        if self.input_baseline.is_none() {
            self.input_baseline = Some(self.latest_frame_inputs());
        }
    }

    /// How many inputs have been sent.
    pub(crate) const fn inputs_sent(&self) -> u64 {
        self.inputs_sent
    }

    /// The `inputs` counter at which a frame reflects every input sent so far.
    pub(crate) fn reflecting_counter(&self) -> u64 {
        self.input_baseline.unwrap_or(0) + self.inputs_sent
    }

    /// The latest frame, if it reflects every input sent so far.
    pub(crate) fn frame_reflecting_inputs(&self) -> Option<&Event> {
        let frame = self
            .events
            .events()
            .iter()
            .rev()
            .find(|event| matches!(event.payload, Payload::Frame { .. }))?;
        let Payload::Frame { inputs, .. } = frame.payload else {
            return None;
        };
        (inputs >= self.reflecting_counter()
            && self.last_input_at.is_none_or(|sent| frame.observed >= sent))
        .then_some(frame)
    }

    /// The `inputs` counter of the latest frame, or 0 before the first one.
    pub(crate) fn latest_frame_inputs(&self) -> u64 {
        self.events
            .events()
            .iter()
            .rev()
            .find_map(|event| match event.payload {
                Payload::Frame { inputs, .. } => Some(inputs),
                _ => None,
            })
            .unwrap_or(0)
    }

    /// How many of the inputs sent the latest frame counts: its `inputs` counter less the
    /// program's own inputs (the baseline), or 0 before the first frame.
    pub(crate) fn latest_frame_inputs_sent(&self) -> u64 {
        self.latest_frame_inputs()
            .saturating_sub(self.input_baseline.unwrap_or(0))
    }

    /// Whether the program marks its frames in the terminal output, as its `hello` said (see
    /// "Frame marks" in the module documentation). `false` for a program from before the marks,
    /// and until the `hello` has been read.
    pub(crate) const fn frame_marks(&self) -> bool {
        matches!(
            self.capabilities,
            Some(Capabilities {
                frame_marks: true,
                ..
            })
        )
    }

    /// Whether the terminal ties its screen to a frame exactly (see [`SCREEN_IS_EXACT`]): its
    /// frame window is zero, so the mark of a frame follows the frame's bytes. A window that is
    /// not zero says that the terminal paints on its own timer (`ConPTY`), and then no rule proves
    /// that a paint is complete.
    pub(crate) const fn screen_is_exact(&self) -> bool {
        self.frame_window.is_zero()
    }

    /// Why nothing may be confirmed from what this program and its terminal show, when nothing
    /// may: a program that does not mark its frames ([`NO_FRAME_MARKS`]), one that does not answer
    /// input barrier requests ([`NO_INPUT_BARRIER`]), or a terminal whose screen is not exact
    /// ([`SCREEN_NOT_EXACT`]). `None` when a deletion may go on to read the screen. Whatever could
    /// confirm a deletion asks this before it sends a key, a Backspace included, and refuses with
    /// the reason when it is not `None`.
    pub(crate) fn deletion_refusal(&self) -> Option<String> {
        if !self.frame_marks() {
            Some(NO_FRAME_MARKS.to_owned())
        } else if !self.input_barrier() {
            Some(NO_INPUT_BARRIER.to_owned())
        } else if !self.screen_is_exact() {
            Some(format!(
                "{SCREEN_NOT_EXACT}; no key was sent. A scenario that deletes runs in-process \
                 under `cargo test`, where no terminal comes between the program and its screen"
            ))
        } else {
            None
        }
    }

    /// The `seq` of the first frame that counts every input sent so far and was observed after
    /// the last of them was written, if the program has drawn one: the frame that shows what the
    /// inputs did. The first is the one to wait for. Frames after it show the same and more, but a
    /// program that keeps drawing never stops producing them.
    pub(crate) fn reflecting_frame_seq(&self) -> Option<u64> {
        let counter = self.reflecting_counter();
        self.events
            .events()
            .iter()
            .find_map(|event| match event.payload {
                Payload::Frame { seq, inputs, .. }
                    if inputs >= counter
                        && self.last_input_at.is_none_or(|sent| event.observed >= sent) =>
                {
                    Some(seq)
                }
                _ => None,
            })
    }

    /// The `seq` of the latest frame the program has reported, if it has reported one.
    pub(crate) fn latest_frame_seq(&self) -> Option<u64> {
        self.events
            .events()
            .iter()
            .rev()
            .find_map(|event| match event.payload {
                Payload::Frame { seq, .. } => Some(seq),
                _ => None,
            })
    }

    /// The `seq` of the first frame among the events from index `start` on, if there is one.
    pub(crate) fn first_frame_from(&self, start: usize) -> Option<u64> {
        self.events
            .events()
            .get(start..)?
            .iter()
            .find_map(|event| match event.payload {
                Payload::Frame { seq, .. } => Some(seq),
                _ => None,
            })
    }

    /// Whether the screen model shows frame `seq`, or a later one: it has read the mark of
    /// that frame, and, where the terminal paints on its own timer (a non-zero
    /// [`Live::frame_window`], `ConPTY`), the paint that follows the mark has come or the window
    /// has passed with nothing painted ([`PtySession::paint_followed_mark`]). Only a program that
    /// marks its frames can ever say so.
    pub(crate) fn screen_shows(&self, seq: u64) -> bool {
        self.session.frame_shown() >= seq
            && (self.frame_window.is_zero()
                || self.session.paint_followed_mark(seq, self.frame_window))
    }

    /// Whether the screen can be believed to show what every input sent so far did: a frame that
    /// counts them all has been drawn and, when the program marks its frames, the screen shows it.
    ///
    /// That is the condition for reading a prompt, a dialog, or the selected item from the screen
    /// after an input. A program that does not mark its frames gives no more evidence than the
    /// frame event, and a protocol that must not rely on less refuses such a program instead
    /// (see [`Drive::confirm_deletion`]).
    pub(crate) fn screen_reflects_inputs(&self) -> bool {
        self.reflecting_frame_seq()
            .is_some_and(|seq| !self.frame_marks() || self.screen_shows(seq))
    }

    /// Whether the screen model shows frame `seq` for a read that a deletion depends on: the
    /// terminal's screen is exact ([`Live::screen_is_exact`]) and the session has read the mark of
    /// `seq`. Never true on a terminal that paints on its own timer, where no paint can be proved
    /// complete: whatever such a read decides, it is not about what the program has. On an exact
    /// terminal it is the mark, as for [`Live::screen_shows`].
    pub(crate) fn screen_surely_shows(&self, seq: u64) -> bool {
        self.screen_is_exact() && self.session.frame_shown() >= seq
    }

    /// What the program has drawn and what the screen shows of it, for failure messages.
    pub(crate) fn frames_summary(&self) -> String {
        let drawn = self
            .latest_frame_seq()
            .map_or_else(|| "no frame yet".to_owned(), |seq| format!("frame {seq}"));
        if self.frame_marks() {
            format!(
                "the program has reported {drawn} and the screen shows frame {}",
                self.session.frame_shown()
            )
        } else {
            format!("the program has reported {drawn} and does not mark its frames")
        }
    }

    /// A summary of the events read so far, for failure messages.
    pub(crate) fn events_summary(&self) -> String {
        let events = self.events.events();
        let latest: Vec<&str> = events
            .iter()
            .rev()
            .take(8)
            .map(|event| event.payload.kind_name())
            .collect();
        format!(
            "{} events were read; the latest were {:?}; {}",
            events.len(),
            latest.into_iter().rev().collect::<Vec<_>>(),
            self.frames_summary()
        )
    }

    /// How the program ended, for failure messages.
    pub(crate) fn exit_summary(&self) -> String {
        self.session
            .exit()
            .map_or_else(|| "it is still running".to_owned(), |exit| exit.describe())
    }
}

/// Why a protocol did not complete: what it waited for, and what was there instead.
#[derive(Debug)]
pub(crate) struct Unmet {
    /// How the wait ended. [`Waited::Ready`] means the program showed or did something other than
    /// what was expected, so there was nothing to wait for any longer.
    pub(crate) waited: Waited<()>,
    /// The cause to report when `waited` is [`Waited::Ready`].
    pub(crate) cause: FailureCause,
    /// What the protocol expected, rendered as text.
    pub(crate) expected: String,
    /// What was found instead.
    pub(crate) observed: String,
}

impl Unmet {
    /// A wait that did not end in success.
    fn wait(waited: Waited<()>, expected: impl Into<String>, observed: impl Into<String>) -> Self {
        Self {
            waited,
            cause: FailureCause::Mismatch,
            expected: expected.into(),
            observed: observed.into(),
        }
    }

    /// The program showed something other than what was expected.
    fn mismatch(expected: impl Into<String>, observed: impl Into<String>) -> Self {
        Self::wait(Waited::Ready(()), expected, observed)
    }

    /// A deletion that was refused: the confirmation key was never sent.
    fn refused(expected: impl Into<String>, observed: impl Into<String>) -> Self {
        Self {
            cause: FailureCause::DeleteRefused,
            ..Self::mismatch(expected, observed)
        }
    }
}

/// How a protocol stopped.
#[derive(Debug)]
pub(crate) enum ProtocolError {
    /// The program did not do what the protocol needed.
    Unmet(Unmet),
    /// The harness could not go on.
    Run(RunError),
}

impl<E: Into<RunError>> From<E> for ProtocolError {
    fn from(error: E) -> Self {
        Self::Run(error.into())
    }
}

impl From<Unmet> for ProtocolError {
    fn from(unmet: Unmet) -> Self {
        Self::Unmet(unmet)
    }
}

/// What a deletion asks to be checked before the confirmation key is sent.
pub(crate) struct DeletionRequest<'a> {
    /// The name of the entry, its last path component.
    pub(crate) name: &'a str,
    /// Whether the entry is a file or a folder.
    pub(crate) kind: EntryKind,
    /// The fixture the entry is in.
    pub(crate) fixture: &'a FixtureRoot,
    /// The fixture-relative paths that must exist, and must not lie in the entry.
    pub(crate) sentinels: &'a [String],
    /// The fixture-relative path the dialog must name exactly. The dialog proves the entry only
    /// down to its name, so without it a same-named entry in another folder would pass.
    pub(crate) relative: &'a str,
    /// The key that confirms the dialog once it is verified.
    pub(crate) confirm_with: ConfirmKey,
}

/// A deletion whose confirmation key was sent.
pub(crate) struct Confirmed {
    /// What the dialog was verified to name.
    pub(crate) verified: Verified,
    /// The dialog, as it was verified.
    // Only the interactive driver reads this, and the driver builds on Unix only.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) dialog: DeleteDialog,
    /// When the confirmation key was written.
    pub(crate) at: Instant,
    /// How many events had been read when it was written: a `deletion_finished` event that
    /// answers it is one of the events after these.
    pub(crate) events_before: usize,
}

/// What a finished deletion reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DeletionFinished {
    /// Entries removed.
    pub(crate) removed: u64,
    /// Entries that could not be removed.
    pub(crate) failed: u64,
}

/// What the exact screen must show, besides no deletion dialog, for a write that could confirm a
/// deletion to be sent after a deletion was requested outside the protocols (see
/// [`Drive::send_confirming`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Needs {
    /// Nothing more: a `key` or `type` step of the scenario's own.
    Nothing,
    /// The filter prompt, open: the text and the Enter of a selection.
    FilterPrompt,
    /// The plain quit prompt, the one that offers `[y] Quit` and lists no waiting check, so that
    /// no deletion is pending beneath it.
    PlainQuitPrompt,
}

impl Needs {
    /// What the screen must show beyond no deletion dialog, as a clause that continues a sentence
    /// about the screen; empty for [`Needs::Nothing`].
    const fn clause(self) -> &'static str {
        match self {
            Self::Nothing => "",
            Self::FilterPrompt => " and the filter prompt open",
            Self::PlainQuitPrompt => " and the plain quit prompt open",
        }
    }

    /// What `screen` shows short of what these needs ask for, if anything: a mismatch, with
    /// `expected` as what was expected, when the filter prompt is not open (for
    /// [`Needs::FilterPrompt`]) or the quit dialog is not the plain one (for
    /// [`Needs::PlainQuitPrompt`]). The screen is already known to show no deletion dialog.
    fn unmet(self, screen: &Screen, expected: &str) -> Option<Unmet> {
        match self {
            Self::Nothing => None,
            Self::FilterPrompt => filter_prompt(screen).is_none().then(|| {
                Unmet::mismatch(
                    expected,
                    format!(
                        "the filter prompt is not open; no key was sent. The header shows:\n{}",
                        screen.rows_text(0, 2)
                    ),
                )
            }),
            Self::PlainQuitPrompt => match dialog_view(screen) {
                DialogView::Other(view) if offers_plain_quit(&view) => None,
                DialogView::Other(view) => Some(Unmet::mismatch(
                    "the quit dialog to offer `[y] Quit`",
                    format!("the dialog offers something else:\n{}", view.text),
                )),
                DialogView::Delete(_) | DialogView::None => Some(Unmet::mismatch(
                    "the quit dialog to offer `[y] Quit`",
                    format!("no quit dialog is open:\n{}", screen.text()),
                )),
            },
        }
    }
}

/// `bytes` as a message names them: a key, or the bytes of one that has no name.
fn describe_write(bytes: &[u8]) -> String {
    match bytes {
        b"\r" => "Enter".to_owned(),
        b"\n" => "a line feed".to_owned(),
        _ => match std::str::from_utf8(bytes) {
            Ok(text) if !text.chars().any(char::is_control) => format!("`{text}`"),
            _ => format!("the bytes {bytes:02x?}"),
        },
    }
}

/// Whoever drives a [`Live`]. Implementors give access to it and say how to pump it; every
/// protocol is a provided method.
pub(crate) trait Drive: Sized {
    /// The program driven.
    fn live(&self) -> &Live;

    /// The program driven.
    fn live_mut(&mut self) -> &mut Live;

    /// Reads all pending terminal output and events, and whatever else the driver samples.
    fn pump(&mut self) -> Result<(), RunError>;

    /// Called after an input was written, for a driver that records what it sends.
    fn input_sent(&mut self, _sent: &Sent) {}

    /// Drives the session until `probe` returns a value, the deadline passes, or the program ends.
    fn wait_until<T>(
        &mut self,
        deadline: Instant,
        mut probe: impl FnMut(&Self) -> Option<T>,
    ) -> Result<Waited<T>, RunError> {
        loop {
            self.pump()?;
            if let Some(value) = probe(self) {
                return Ok(Waited::Ready(value));
            }
            if self.live().session.finished() {
                return Ok(Waited::Exited);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(Waited::TimedOut);
            }
            self.live_mut()
                .session
                .wait_activity(deadline.saturating_duration_since(now).min(POLL_INTERVAL))?;
        }
    }

    /// Writes one terminal input event and records it.
    fn send_input(&mut self, bytes: &[u8]) -> Result<Instant, RunError> {
        let sent = self.live_mut().send_input(bytes)?;
        self.input_sent(&sent);
        Ok(sent.at)
    }

    /// Writes `bytes`, which a protocol or a step wants to send, after checking what they could
    /// meet if they could confirm a deletion (see "Raw deletion requests" in the module
    /// documentation).
    ///
    /// Bytes that cannot be read as a confirmation ([`Live::reading_of`]: `y`, `Y`, Enter, a line
    /// feed, or a write that continues an escape sequence that an earlier write began) are sent at
    /// once, as [`Drive::send_input`] sends them, and so is everything in a run in which no write
    /// of the scenario's own could be read as a request for a deletion
    /// ([`Live::raw_deletion_requested`]): every fast path stays. In an engaged run the bytes are
    /// sent only after a barrier has been answered and the screen shows the frame that answers
    /// it, and only if that screen shows no deletion dialog and what `needs` asks for. Anything
    /// else fails with [`FailureCause::DeleteRefused`] (a screen that is not exact, a program that
    /// marks no frames or answers no barrier, the escape byte and a confirmation in one write, a
    /// write that continues an escape sequence that is still open, a deletion dialog) or with a
    /// mismatch (`needs` is not met), and nothing is sent.
    fn send_confirming(
        &mut self,
        bytes: &[u8],
        deadline: Instant,
        needs: Needs,
    ) -> Result<Instant, ProtocolError> {
        if !self.live().raw_deletion_requested() || !self.live().reading_of(bytes).confirmation {
            return Ok(self.send_input(bytes)?);
        }
        let write = describe_write(bytes);
        let expected = format!(
            "the screen to show no deletion dialog{} before {write} is sent",
            needs.clause()
        );
        self.pump()?;
        if let Some(reason) = self.live().deletion_refusal() {
            return Err(Unmet::refused(
                expected,
                format!(
                    "a key that asked for a deletion was written outside a `delete` step, so \
                     {write} could confirm a dialog that nothing verified: {reason}"
                ),
            )
            .into());
        }
        if is_escape_then_confirmation(bytes) {
            return Err(Unmet::refused(
                expected,
                format!(
                    "{write} is the escape byte and a key that could confirm a deletion, in one \
                     write, and a program may read it as two inputs: the escape closes a prompt \
                     that covers a deletion dialog and the key confirms what it uncovers, and no \
                     barrier can come between the two bytes. Send `esc`, a `settle`, and then the \
                     key as steps of their own; no key was sent"
                ),
            )
            .into());
        }
        if self.live().holds_an_escape_sequence() {
            return Err(Unmet::refused(
                expected,
                format!(
                    "{write} would continue an escape sequence that an earlier write began and \
                     nothing has finished (`alt+[` is the start of one): a program joins the \
                     bytes of such a sequence across writes, so what follows can finish it as a \
                     key that confirms a deletion (`ESC [ 1 2 1 u` is `y`), and no barrier can \
                     be written behind it, because the program takes the barrier for a part of \
                     the sequence and never answers it. Send each key as one `key` step; no key \
                     was sent"
                ),
            )
            .into());
        }
        let answered = self.barrier(deadline)?;
        if !matches!(answered, Waited::Ready(())) {
            return Err(Unmet::wait(
                answered,
                format!(
                    "the program to answer an input barrier written before {write}, and the \
                     screen to show the frame that answers it"
                ),
                format!(
                    "the screen shows:\n{}\n{}",
                    self.live().session.screen().text(),
                    self.live().frames_summary()
                ),
            )
            .into());
        }
        let screen = self.live().session.screen();
        if deletion_dialog_visible(screen) {
            return Err(Unmet::refused(
                expected,
                format!(
                    "a deletion dialog is open, and a key that asked for a deletion was written \
                     outside a `delete` step, so {write} could confirm a dialog that nothing \
                     verified; no key was sent:\n{}",
                    screen.text()
                ),
            )
            .into());
        }
        if let Some(unmet) = needs.unmet(screen, &expected) {
            return Err(unmet.into());
        }
        Ok(self.send_input(bytes)?)
    }

    /// Waits for `duration` to pass while pumping the session. Returns `true` if the program
    /// ended first. Unlike every other wait, this one has no condition to satisfy: letting time
    /// pass quietly, with nothing sent, is the point, so reaching the deadline is the wait
    /// succeeding, not timing out.
    fn wait_quietly(&mut self, duration: Duration) -> Result<bool, RunError> {
        let waited = self.wait_until(Instant::now() + duration, |_| None::<()>)?;
        Ok(matches!(waited, Waited::Exited))
    }

    /// Asks the program to say that it has read everything written to it so far, and waits for the
    /// answer and for the screen to show the frame that answers it (see "The input barrier" in the
    /// module documentation).
    ///
    /// A request that an earlier call wrote and the program never answered is waited for first,
    /// within the same `deadline`: with two requests outstanding, the answer to the old one could
    /// be taken for the answer to the new one. The wait is exact, never a guess: it ends when the
    /// frame that answers has been read off the terminal ([`Live::screen_surely_shows`]), when
    /// `deadline` passes, or when the program ends. The screen of a terminal that paints on its
    /// own timer never shows a frame surely, so what needs this on such a terminal is refused
    /// before it asks ([`Live::deletion_refusal`]).
    fn barrier(&mut self, deadline: Instant) -> Result<Waited<()>, RunError> {
        self.pump()?;
        if self.live().barrier_outstanding() {
            let answered =
                self.wait_until(deadline, |driver| driver.live().barrier_answer_seq())?;
            if !matches!(answered, Waited::Ready(_)) {
                return Ok(answered.discard());
            }
        }
        self.live_mut().send_barrier()?;
        let answered = self.wait_until(deadline, |driver| driver.live().barrier_answer_seq())?;
        let Waited::Ready(seq) = answered else {
            return Ok(answered.discard());
        };
        self.wait_until(deadline, |driver| {
            driver.live().screen_surely_shows(seq).then_some(())
        })
    }

    /// Waits until the screen model shows frame `seq` of the program, which a wait has just seen
    /// reported, so that what comes after can read the screen once.
    ///
    /// Every protocol that ends on a frame event (`settle`, `delete`, `resize`) ends here. The event
    /// says that the program drew; the terminal still has to deliver what it drew. A program that
    /// marks its frames says when it has: on a Unix pseudo-terminal the screen shows the frame once
    /// the session has read its mark, so this waits for that, exactly, until `deadline`
    /// (`Waited::TimedOut` when the mark does not come, and `Waited::Exited` when the program ends
    /// without it). On a terminal that paints on its own timer (a non-zero [`Live::frame_window`],
    /// `ConPTY`) the mark comes before the paint of its frame, so this also waits for that paint,
    /// or for the window to pass with nothing painted ([`Live::screen_shows`]), and then reads on
    /// until the output has been quiet for [`SETTLE_QUIET`], but for no longer than
    /// [`SETTLE_LIMIT`], because a paint can come in pieces.
    ///
    /// A program that does not mark its frames says nothing of the kind. The terminal may hold the
    /// frame back for the whole of [`Live::frame_window`], so that long is read first, with the
    /// events and the session still polled. Then comes the bounded tail: output that is still
    /// arriving is read until it has been quiet for [`SETTLE_QUIET`], but for no longer than
    /// [`SETTLE_LIMIT`]. That is a guess, which is why nothing that could confirm a deletion
    /// relies on it.
    fn catch_up(&mut self, seq: u64, deadline: Instant) -> Result<Waited<()>, RunError> {
        if self.live().frame_marks() {
            let shown = self.wait_until(deadline, |driver| {
                driver.live().screen_shows(seq).then_some(())
            })?;
            // A console host that paints on its own timer paints in pieces too.
            if matches!(shown, Waited::Ready(()))
                && !self.live().frame_window.is_zero()
                && Instant::now() < deadline
            {
                self.live_mut().session.drain(SETTLE_QUIET, SETTLE_LIMIT)?;
                self.pump()?;
            }
            return Ok(shown);
        }
        let window = self.live().frame_window;
        self.wait_quietly(window)?;
        self.live_mut().session.drain(SETTLE_QUIET, SETTLE_LIMIT)?;
        self.pump()?;
        Ok(Waited::Ready(()))
    }

    /// Waits for the first frame among the events from index `start` on, then until the screen
    /// shows it ([`Drive::catch_up`]).
    fn wait_for_frame_from(
        &mut self,
        start: usize,
        deadline: Instant,
    ) -> Result<Waited<()>, RunError> {
        let waited = self.wait_until(deadline, |driver| driver.live().first_frame_from(start))?;
        match waited {
            Waited::Ready(seq) => self.catch_up(seq, deadline),
            other => Ok(other.discard()),
        }
    }

    /// Waits until the screen shows the latest frame the program has reported by now
    /// ([`Drive::catch_up`]). The frame is fixed when the wait begins, so a program that keeps
    /// drawing cannot keep the wait from ending. With no frame reported there is nothing to
    /// wait for.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn catch_up_to_latest(&mut self, deadline: Instant) -> Result<Waited<()>, RunError> {
        match self.live().latest_frame_seq() {
            Some(seq) => self.catch_up(seq, deadline),
            None => Ok(Waited::Ready(())),
        }
    }

    /// Waits until the program has drawn a frame that reflects every input sent so far, then
    /// until the screen shows it ([`Drive::catch_up`]).
    ///
    /// This is the pseudo-terminal meaning of `settle`. It never waits for the screen to go
    /// quiet: a program that keeps animating settles as soon as its frame counter catches up. A
    /// key that changes nothing draws no frame, so a settle after one waits in vain.
    fn settle_until(&mut self, deadline: Instant) -> Result<Waited<()>, RunError> {
        let waited = self.wait_until(deadline, |driver| driver.live().reflecting_frame_seq())?;
        match waited {
            Waited::Ready(seq) => self.catch_up(seq, deadline),
            other => Ok(other.discard()),
        }
    }

    /// Waits for a frame that reflects every input so far, shown on the screen, and a filter
    /// prompt that `accept`s.
    fn wait_filter_prompt(
        &mut self,
        deadline: Instant,
        accept: impl Fn(&FilterPrompt) -> bool,
    ) -> Result<Waited<FilterPrompt>, RunError> {
        self.wait_until(deadline, |driver| {
            driver.live().screen_reflects_inputs().then_some(())?;
            filter_prompt(driver.live().session.screen()).filter(|prompt| accept(prompt))
        })
    }

    /// Opens the filter, types `name` (`name_keys` is its encoding, one entry per character),
    /// applies it, and asserts that the inspector shows exactly that entry.
    ///
    /// The filter opens with the previous filter's text in place, so this reads the prompt and
    /// erases what is there. It checks the prompt after each stage instead of assuming the keys
    /// did what they usually do.
    fn select_entry(
        &mut self,
        name: &str,
        name_keys: &[Vec<u8>],
        deadline: Instant,
    ) -> Result<(), ProtocolError> {
        let header = |driver: &Self| driver.live().session.screen().rows_text(0, 2);

        self.send_input(b"/")?;
        let opened = self.wait_filter_prompt(deadline, |_| true)?;
        let Waited::Ready(prompt) = opened else {
            return Err(Unmet::wait(
                opened.discard(),
                "the filter prompt to open",
                format!("the header shows:\n{}", header(self)),
            )
            .into());
        };

        for _ in prompt.input.chars() {
            self.send_input(&BACKSPACE)?;
        }
        if !prompt.input.is_empty() {
            let erased = self.wait_filter_prompt(deadline, |prompt| prompt.input.is_empty())?;
            if !matches!(erased, Waited::Ready(_)) {
                return Err(Unmet::wait(
                    erased.discard(),
                    "the previous filter to be erased",
                    format!("the header shows:\n{}", header(self)),
                )
                .into());
            }
        }

        for key in name_keys {
            self.send_confirming(key, deadline, Needs::FilterPrompt)?;
        }
        let typed = self.wait_filter_prompt(deadline, |prompt| {
            prompt.input == name && prompt.error.is_none()
        })?;
        if !matches!(typed, Waited::Ready(_)) {
            return Err(Unmet::wait(
                typed.discard(),
                format!("the filter prompt to read {name:?}"),
                format!(
                    "the prompt reads {:?}; the header shows:\n{}",
                    filter_prompt(self.live().session.screen()).map(|prompt| prompt.input),
                    header(self)
                ),
            )
            .into());
        }

        self.send_confirming(&ENTER, deadline, Needs::FilterPrompt)?;
        let selected = self.wait_until(deadline, |driver| {
            driver.live().screen_reflects_inputs().then_some(())?;
            if filter_prompt(driver.live().session.screen()).is_some() {
                return None;
            }
            match inspector(driver.live().session.screen()) {
                Inspector::Item(item) if item.name == name => Some(item),
                _ => None,
            }
        })?;
        let Waited::Ready(_) = selected else {
            let screen = self.live().session.screen();
            let shown = match inspector(screen) {
                Inspector::Item(item) => {
                    format!("the inspector shows {:?} ({})", item.name, item.kind)
                }
                Inspector::NothingSelected => "the inspector shows no selection".to_owned(),
                Inspector::NotShown => "the inspector is not drawn".to_owned(),
            };
            let expected = format!("the inspector to show the selected item {name:?}");
            // An entry that is selected but is not the requested one is a wrong selection, not
            // merely a slow screen.
            if matches!(inspector(screen), Inspector::Item(_))
                && matches!(selected, Waited::TimedOut)
            {
                return Err(Unmet::mismatch(expected, shown).into());
            }
            return Err(Unmet::wait(selected.discard(), expected, shown).into());
        };
        Ok(())
    }

    /// Applies an empty filter, so that the filter [`Drive::select_entry`] types next is a change.
    ///
    /// The program selects nothing when it is given the filter it already has, so a selection
    /// through the filter fails while the filter is still the entry's name from an earlier
    /// selection. A driver that cannot tell what the filter is clears it first. An empty prompt
    /// means no filter is applied, and closing it changes nothing.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn clear_filter(&mut self, deadline: Instant) -> Result<(), ProtocolError> {
        let header = |driver: &Self| driver.live().session.screen().rows_text(0, 2);

        self.send_input(b"/")?;
        let opened = self.wait_filter_prompt(deadline, |_| true)?;
        let Waited::Ready(prompt) = opened else {
            return Err(Unmet::wait(
                opened.discard(),
                "the filter prompt to open",
                format!("the header shows:\n{}", header(self)),
            )
            .into());
        };
        let closing: &[u8] = if prompt.input.is_empty() {
            b"\x1b"
        } else {
            for _ in prompt.input.chars() {
                self.send_input(&BACKSPACE)?;
            }
            let erased = self.wait_filter_prompt(deadline, |prompt| prompt.input.is_empty())?;
            if !matches!(erased, Waited::Ready(_)) {
                return Err(Unmet::wait(
                    erased.discard(),
                    "the previous filter to be erased",
                    format!("the header shows:\n{}", header(self)),
                )
                .into());
            }
            &ENTER
        };
        self.send_confirming(closing, deadline, Needs::FilterPrompt)?;
        let closed = self.wait_until(deadline, |driver| {
            driver.live().screen_reflects_inputs().then_some(())?;
            filter_prompt(driver.live().session.screen())
                .is_none()
                .then_some(())
        })?;
        if !matches!(closed, Waited::Ready(())) {
            return Err(Unmet::wait(
                closed.discard(),
                "the filter prompt to close",
                format!("the header shows:\n{}", header(self)),
            )
            .into());
        }
        Ok(())
    }

    /// The deletion protocol up to its confirmation: presses Backspace, reads the confirmation
    /// dialog, checks it with [`verify`] and against the requested path, and presses the
    /// requested confirmation key (`y` or Enter) only when the dialog names exactly the requested
    /// entry. Otherwise the result is an [`Unmet`] with [`FailureCause::DeleteRefused`] and no
    /// confirmation key is sent.
    ///
    /// Before any key is sent, a Backspace included, the program and its terminal must be ones a
    /// deletion can be confirmed with ([`Live::deletion_refusal`]): a program that does not mark
    /// its frames cannot say when the screen shows the dialog it has ([`NO_FRAME_MARKS`]), one
    /// that does not answer input barrier requests cannot say that it has read what was written
    /// to it ([`NO_INPUT_BARRIER`]), and a terminal that paints on its own timer (`ConPTY`, on
    /// Windows) cannot tie its screen to a frame at all ([`SCREEN_NOT_EXACT`]), whatever is
    /// waited for. Each is refused with [`FailureCause::DeleteRefused`] and no key sent.
    ///
    /// The dialog is then read after a barrier written behind the Backspace ([`Drive::barrier`]),
    /// from a screen that shows the frame that answers it. The program has then read the
    /// Backspace and everything before it, however the terminal cut those bytes into events, and
    /// the screen shows what it did: a dialog opens on an input and never by itself, so what the
    /// screen shows about dialogs is what a confirmation key would meet, and the confirmation key
    /// is the next thing written (see "The input barrier" in the module documentation). A
    /// program that answers no barrier gets no confirmation key.
    ///
    /// Right before the confirmation key, the root is checked once more for its ownership
    /// marker: the check at the start of a run says nothing of what happened to the root since, and
    /// a root that is no longer a fixture is not one to confirm a deletion in.
    ///
    /// This is the one place the harness sends the confirmation key of a deletion: the scenario
    /// `delete` step and `cargo xtask tui delete` both call it, so every deletion passes the same
    /// check.
    fn confirm_deletion(
        &mut self,
        request: &DeletionRequest<'_>,
        deadline: Instant,
    ) -> Result<Confirmed, ProtocolError> {
        let expected = format!(
            "the deletion dialog to name the {} {:?} under {}",
            request.kind,
            request.name,
            request.fixture.path().display()
        );

        self.pump()?;
        if let Some(reason) = self.live().deletion_refusal() {
            return Err(Unmet::refused(expected, reason).into());
        }
        self.send_input(&BACKSPACE)?;
        let answered = self.barrier(deadline)?;
        if !matches!(answered, Waited::Ready(())) {
            return Err(Unmet::wait(
                answered,
                "the program to answer an input barrier written behind the Backspace, and the \
                 screen to show the frame that answers it",
                format!(
                    "the screen shows:\n{}\n{}",
                    self.live().session.screen().text(),
                    self.live().frames_summary()
                ),
            )
            .into());
        }
        let dialog = match dialog_view(self.live().session.screen()) {
            DialogView::Delete(dialog) => dialog,
            DialogView::Other(other) => {
                return Err(Unmet::refused(
                    expected,
                    format!(
                        "the dialog `{}` is open instead:\n{}",
                        other.title, other.text
                    ),
                )
                .into());
            }
            DialogView::None => {
                return Err(Unmet::mismatch(
                    "a deletion dialog to open",
                    format!(
                        "no dialog is open, although the program has read the Backspace and \
                         everything before it:\n{}\n{}",
                        self.live().session.screen().text(),
                        self.live().frames_summary()
                    ),
                )
                .into());
            }
        };
        let verified = verify(
            &dialog,
            request.name,
            request.kind,
            request.fixture,
            request.sentinels,
        )
        .map_err(|reason| {
            Unmet::refused(
                expected.clone(),
                format!("{reason}\nthe dialog reads:\n{}", dialog.view.text),
            )
        })?;
        if verified.relative != request.relative {
            return Err(Unmet::refused(
                expected,
                format!(
                    "the dialog deletes `{}`, but the request is for `{}`\nthe dialog \
                     reads:\n{}",
                    verified.relative, request.relative, dialog.view.text
                ),
            )
            .into());
        }
        // The marker is the last thing looked at, right before the key that deletes.
        request.fixture.verify_owned().map_err(|error| {
            Unmet::refused(
                expected,
                format!(
                    "the fixture root is no longer owned by the harness: {error}; no \
                     confirmation key was sent"
                ),
            )
        })?;

        // Every check passed. Only now is the confirmation key sent.
        let events_before = self.live().events.events().len();
        let confirm: &[u8] = match request.confirm_with {
            ConfirmKey::Y => b"y",
            ConfirmKey::Enter => &ENTER,
        };
        let at = self.send_input(confirm)?;
        Ok(Confirmed {
            verified,
            dialog,
            at,
            events_before,
        })
    }

    /// Waits for the deletion confirmed after `events_before` events to report `deletion_finished`
    /// and for the frame drawn after it, so the screen already shows the result.
    ///
    /// The program starts rebuilding its map in the same turn that reports the deletion, so a
    /// frame drawn after that report shows the result and everything the deletion set going. The
    /// screen read before it can still show the map as it was.
    fn wait_deletion_finished(
        &mut self,
        events_before: usize,
        deadline: Instant,
    ) -> Result<DeletionFinished, ProtocolError> {
        let finished = self.wait_until(deadline, |driver| {
            driver.live().events.events()[events_before..]
                .iter()
                .position(|event| matches!(event.payload, Payload::DeletionFinished { .. }))
                .map(|offset| events_before + offset)
        })?;
        let Waited::Ready(finished_at) = finished else {
            return Err(Unmet::wait(
                finished.discard(),
                "the deletion to finish (a `deletion_finished` event)",
                self.live().events_summary(),
            )
            .into());
        };
        let Payload::DeletionFinished { removed, failed } =
            self.live().events.events()[finished_at].payload
        else {
            return Err(RunError::Protocol(
                "the event found as `deletion_finished` is another".to_owned(),
            )
            .into());
        };

        let shown = self.wait_for_frame_from(finished_at + 1, deadline)?;
        match shown {
            Waited::Ready(()) => Ok(DeletionFinished { removed, failed }),
            other => Err(Unmet::wait(
                other.discard(),
                "a frame after the deletion finished, shown on the screen",
                self.live().events_summary(),
            )
            .into()),
        }
    }

    /// Waits for a frame that shows the confirmation dialog closed: the program left
    /// `DeleteConfirm` for the planning and execution path in the same turn, so the deletion has
    /// started. Nothing here waits for it to finish.
    fn wait_dialog_closed(&mut self, deadline: Instant) -> Result<(), ProtocolError> {
        let closed = self.wait_until(deadline, |driver| {
            driver.live().screen_reflects_inputs().then_some(())?;
            matches!(
                dialog_view(driver.live().session.screen()),
                DialogView::None
            )
            .then_some(())
        })?;
        match closed {
            Waited::Ready(()) => Ok(()),
            other => Err(Unmet::wait(
                other.discard(),
                "the deletion dialog to close after the confirmation",
                format!("the screen shows:\n{}", self.live().session.screen().text()),
            )
            .into()),
        }
    }

    /// Presses `q` and waits for the quit dialog, which it returns. The dialog is the one the
    /// program built after the key: a `quit_prompt` event after the key, and a dialog on the
    /// screen that a frame reflecting the key drew.
    fn request_quit(&mut self, deadline: Instant) -> Result<BoxView, ProtocolError> {
        let events_before = self.live().events.events().len();
        self.send_input(b"q")?;
        let opened = self.wait_until(deadline, |driver| {
            // The frame event that counts the key, not its mark: the key that follows only quits,
            // so nothing destructive depends on the screen being exact, and waiting for the end of
            // the frame would press it after the program has begun drawing the next one, which a
            // quit on a slow terminal then has to wait for (`quit_ms`).
            driver.live().frame_reflecting_inputs()?;
            let prompted = driver.live().events.events()[events_before..]
                .iter()
                .any(|event| matches!(event.payload, Payload::QuitPrompt));
            if !prompted {
                return None;
            }
            match dialog_view(driver.live().session.screen()) {
                DialogView::Other(view) => Some(view),
                _ => None,
            }
        })?;
        match opened {
            Waited::Ready(view) => Ok(view),
            other => Err(Unmet::wait(
                other.discard(),
                "the quit dialog to open",
                format!("the screen shows:\n{}", self.live().session.screen().text()),
            )
            .into()),
        }
    }
}
