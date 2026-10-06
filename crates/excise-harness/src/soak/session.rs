//! One session in a pseudo-terminal: the scan, the keys, the largest folder, the quit.
//!
//! A session starts `excise` on the soak's root, in a pseudo-terminal, with the isolated
//! environment of every run (`safety::isolated_env`: a scratch `HOME`, configuration, working
//! directory, scan store, and temporary directory, and the test event channel), under the
//! `default` or the `deterministic` profile. The only argument is the root: never a flag, and above
//! all never `--disable-delete-confirmation`. Then:
//!
//! 1. **First frame.** The wait for the first `frame` event, and for the screen to show it: the
//!    event and the terminal are two routes, and nothing reads the screen or sends a key before
//!    the screen holds the frame. The scan's bound ([`Limits::scan`]) is counted from the event,
//!    once, and the keys and the wait for the end of the scan share it.
//! 2. **Keys during the scan.** One `Esc` at a time, each sent when the previous one has been
//!    answered by a frame, until the scan ends. At the root `Esc` goes up a folder that is not
//!    there: the program flashes the path and moves nothing, which is a frame drawn for an
//!    input. It is the probe because it does not move the cursor, and a key that does (an arrow, a
//!    Vim letter) locks the cursor for the session: the program then no longer puts it on the
//!    largest entry when the scan ends, which is where the next phase wants it. Each key's latency
//!    is the write to the first frame whose `inputs` counter covers it (the same figure the
//!    scenarios call `input_to_frame_*`), and the screen is read once the frame's mark has been.
//!    The screen is also looked at for a dialog before each key, the first included: an `Esc`
//!    dismisses an error dialog, so one that came up between two keys would be gone from the
//!    frame the key draws.
//! 3. **Complete.** The wait for the scan to be over and shown: the `scan_complete` event, and
//!    then a frame, drawn after the event, that the screen shows and whose header badge says that
//!    the scan of what is shown has ended. Every read of a frame (the badge, the inspector, a
//!    dialog) is made of the screen as the frame's mark left it ([`Driver::shown`]), never of the
//!    screen model itself: the read of the terminal that brings a mark can bring the first output
//!    of the next redraw as well, and then the model holds half a frame, a badge half drawn. The
//!    wait ends with the bound on the scan. The badge is `COMPLETE`, or the label of an uncertain
//!    result (`NEEDS REVIEW`, `READ ERROR`, `OTHER DEVICE`, ...), whatever it is: a real home
//!    directory nearly always holds folders the program cannot read, and a wait for the literal
//!    `COMPLETE` would wait for something that never comes. A label other than `COMPLETE` is a
//!    quirk ([`QuirkKind::UncertainScan`]): counted by kind in `summary.json`, its text only in
//!    `quirks.txt`.
//! 4. **A folder.** At the end of the scan the program puts the cursor on the largest entry. If
//!    the inspector says it is a folder, `Enter` opens it, and `Esc` goes back up. Both are
//!    timed, from the key to a screen that shows a frame with the folder (or the root) and a badge
//!    that says its scan ended; neither is read from a redraw in progress. The folder is told from
//!    the root by the path in the header. Where the header cuts the root's path in the middle that
//!    text is no identity (two paths can be cut to the same text), and the inspector is used as
//!    well: the pane shows something else than the folder (or nothing, in a folder with nothing
//!    in it), and the folder again when the way up is made. An entry is told by its whole pane
//!    ([`SelectedItem::pane`]: name, sizes, item check) and never by its name, which the program
//!    cuts to fit and which two entries can share. If the largest entry is a file, the cursor is
//!    first moved to a folder with the arrow keys. The map is a treemap, not a list, so the walk
//!    ([`Walk`]) tries each of the four arrow keys from every entry the cursor reaches, goes back
//!    along the keys it knows to an entry that still has one to try, and tells a folder by the
//!    inspector, for at most [`MAX_WALK`] keys, and within the one bound that the walk and the
//!    opening share ([`Limits::drill`]). The first folder it reaches is the one opened. A root
//!    where it has been all around and reached none (files alone) is not drilled, and that is no
//!    quirk; a walk that was cut short is not drilled and is one ([`QuirkKind::DrillSkipped`]).
//! 5. **The quit.** The shared quit path of `runner::live`: `q`, the frame event that counts it and
//!    the quit prompt, and, if the prompt offers the plain quit, `y`. The prompt and the exit
//!    share one bound ([`Limits::quit`]).
//!
//! Every key goes through one choke point, [`Driver::send_input`], which refuses anything outside
//! the allowlist of [`Key`] before it writes a byte. The protocols of `runner::live` that the
//! driver shares (`request_quit`) send their keys through it too, because they call it, and the
//! protocols that could send a Backspace (`confirm_deletion`, `select_entry`) are refused by it
//! if anything ever called them. The one protocol that writes around it, `Drive::barrier`, which
//! asks the program to say that it has read everything with a byte that is not a key, is
//! overridden to refuse: the driver has no way to write to the program that does not pass the
//! allowlist. Two writes to the program are not keys and do not pass it, and neither is the
//! soak's: the pseudo-terminal layer answers a cursor position request in the program's output
//! (`ESC [ 6 n`) with `ESC [ row ; col R` (`PtySession::pump`, which `ConPTY` needs at every
//! start), and dropping the session, after the program was killed, closes the terminal with a
//! line feed and an end-of-file. Neither can form a deletion key, and a test pins the shape of
//! the answer.
//!
//! On a terminal whose screen is not exact (Windows: `ConPTY` paints on its own timer) latency is
//! taken from frame events, which the program writes itself, screen reads are lenient, and no
//! decision rests on the screen: `Enter` is sent whatever the inspector says, and whether a folder
//! opened is a measurement, not a condition.
//!
//! The person's interrupt is looked at where the session reads the program ([`Driver::pump`],
//! which every wait calls before it asks its question). It kills the program's process group and
//! ends the wait with an error, so that a wait cannot answer from a read that also brought the
//! kill and let the session send a key to a program that is gone. A session whose program was
//! killed for the interrupt is an interrupted one ([`End::Interrupted`]), and a killed one, and
//! no harness error. The end of every session kills the program's process group if it is still
//! there, and a program that is still there after that, one stuck where a signal does not reach
//! it, is a harness error: the next session would start beside it.
//!
//! The program is one that the soak has to end, and the interrupt is told so for as long as it
//! is ([`Interrupt::supervising`](super::Interrupt::supervising)): the guard is taken before the
//! program is started, so that no program starts once the person has asked to stop, and held by
//! the driver until `end_the_program` has ended the program and waited for it, or given up on it.
//! It is dropped there, before the rest of the output is read, the recording is finished, the
//! scratch area is looked at, or the area is removed. That work can block for as long as a file
//! system takes to answer, and a second press ends the command only once no program is held.

use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use crate::{
    events::Payload,
    metrics::Recorder,
    pty::{
        ExitInfo, PtySession, Screen, SpawnSpec, TerminalModes,
        ui::{
            DialogView, HeaderState, Inspector, SelectedItem, deletion_dialog_visible, dialog_view,
            header_path, header_state, inspector, offers_plain_quit,
        },
    },
    report::{QuirkKind, SoakExit, SoakPhase, tui::ExitVia},
    runner::{
        RunError,
        live::{Drive, Live, ProtocolError, Sent, Waited},
    },
    safety::{Scratch, ScratchError, isolated_env},
    scenario::Profile,
};

use super::{
    Context, SoakError, Supervision,
    headless::{ScanResidue, look_at_residue},
    keys::{Key, Refusal},
    quirks::QuirkLog,
    store_watch::StoreWatch,
    walk::{Next, Walk},
};

/// The terminal the soak runs in, in columns and rows: the harness's default.
const TERMINAL: (u16, u16) = (120, 40);
/// How long the soak lets a key's frame go by before it sends the next probe.
const PROBE_EVERY: Duration = Duration::from_secs(1);
/// How many probes the soak sends during a scan at most: two minutes of it.
const MAX_PROBES: u32 = 120;
/// How many arrow keys the walk to a folder presses at most, when the cursor starts on a file.
const MAX_WALK: u32 = 48;
/// How often the scan-store directory is sampled for its peak size. A real scan's store is bigger
/// than a fixture's, so this is slower than the scenario runner's 50 ms.
const STORE_SAMPLE_EVERY: Duration = Duration::from_millis(250);
/// How long a drill that decided nothing on the screen waits for the folder to show, on a terminal
/// whose screen is not exact: the entry under the cursor may not be a folder.
const INEXACT_DRILL_WAIT: Duration = Duration::from_secs(5);
/// A gap between frames, while the program is busy, that is worth a line in the quirk log.
const STALL_QUIRK_MS: f64 = 1_000.0;
/// How long the output of a program that has ended is read once more, and for how long at most.
const DRAIN_QUIET: Duration = Duration::from_millis(20);
const DRAIN_LIMIT: Duration = Duration::from_millis(500);

/// What the program puts in the middle of a path that does not fit where it is shown, in place of
/// what it cuts: `[...]` in a width that is even, `[..]` in one that is odd
/// (`truncate_middle`, `src/ui/format/truncate.rs`).
const CUT_MARKERS: [&str; 2] = ["[...]", "[..]"];

/// Whether a path as the header shows it may have been cut in the middle: it holds what the
/// program puts there when it cuts one ([`CUT_MARKERS`]). A path that does not may be taken for
/// the whole path. One that does is not an identity: two different paths can be cut to the same
/// text. (A folder that has such a marker in its name is taken for cut, which only means that the
/// entry in the inspector is used as well.)
fn header_is_cut(path: &str) -> bool {
    CUT_MARKERS.iter().any(|marker| path.contains(marker))
}

/// Whether `screen` shows what is inside the folder whose pane was `entered` when the cursor was
/// on it: the inspector shows an entry that is not the folder (by its whole pane, never by its
/// name, which is cut to fit, and which a folder can share with the entry in it), or, for a
/// folder with nothing in it, shows no entry at all.
fn shows_the_inside_of(screen: &Screen, entered: &str) -> bool {
    match inspector(screen) {
        Inspector::Item(item) => item.pane != entered,
        Inspector::NothingSelected => true,
        Inspector::NotShown => false,
    }
}

/// Whether `screen` shows the cursor on the folder whose pane was `entered`: the root as it was
/// before the folder was opened, and as it is again once the way back up has been made.
fn shows_the_cursor_on(screen: &Screen, entered: &str) -> bool {
    matches!(inspector(screen), Inspector::Item(item) if item.pane == entered)
}

/// The dialog on `screen`, if there is one: its kind of quirk, what tells it from another dialog,
/// and what to say of it `when` it was seen.
///
/// Text that reads as a deletion dialog is a quirk of its own kind. Nothing the soak does can open
/// one (it never sends Backspace), so it is a name in the tree, or a program that is not the one
/// under test.
fn dialog_on(screen: &Screen, when: &str) -> Option<(QuirkKind, String, String)> {
    if deletion_dialog_visible(screen) {
        Some((
            QuirkKind::DialogText,
            "text of a deletion dialog".to_owned(),
            format!(
                "the screen shows text that reads as a deletion dialog {when}: the soak asks for \
                 no deletion, so it is a name in the tree, or the program is not the one under \
                 test, and `Enter` and `y` are not sent while it shows"
            ),
        ))
    } else if let DialogView::Other(view) = dialog_view(screen) {
        Some((
            QuirkKind::ErrorDialog,
            view.text.clone(),
            format!("a dialog is on the screen {when}: {}", view.text),
        ))
    } else {
        None
    }
}

/// Whether `error` is the choke point refusing a key because the screen shows text that reads as
/// a deletion dialog ([`Refusal::DeletionDialog`]).
fn refused_for_dialog_text(error: &RunError) -> bool {
    matches!(
        error,
        RunError::Io { source, .. }
            if source.get_ref().and_then(|inner| inner.downcast_ref::<Refusal>())
                == Some(&Refusal::DeletionDialog)
    )
}

/// How a phase, and with it the session, ended early.
#[derive(Debug)]
enum Stop {
    /// The bound of a phase passed.
    Timeout(SoakPhase),
    /// The program ended before the phase was over.
    Exited(SoakPhase),
    /// The quit prompt did not open, or did not offer the plain quit.
    QuitRefused,
    /// The quit prompt opened, and `y` was not sent because the screen shows text that reads as a
    /// deletion dialog: the program is killed instead of quit, which is no error of the harness.
    QuitNotConfirmed,
    /// The bound on the whole run passed.
    RunBound,
    /// The person interrupted the run.
    Interrupted,
    /// The harness could not go on.
    Error(RunError),
}

impl From<RunError> for Stop {
    fn from(error: RunError) -> Self {
        Self::Error(error)
    }
}

/// What the soak knows when a session is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum End {
    /// The session is over and the run goes on.
    Session,
    /// The bound on the whole run passed.
    RunBound,
    /// The person interrupted the run.
    Interrupted,
}

/// What a session's stop says about the run.
struct Stopped {
    end: End,
    timed_out_phase: Option<SoakPhase>,
    error: Option<RunError>,
}

/// A session, after it ended.
#[derive(Debug)]
pub(super) struct TuiRun {
    pub round: u32,
    pub profile: Profile,
    pub exit: SoakExit,
    pub timed_out_phase: Option<SoakPhase>,
    pub terminal_restored: bool,
    /// What the program left in its scratch area. Its names are local.
    pub residue: ScanResidue,
    pub drilled: bool,
    pub metrics: BTreeMap<String, f64>,
    /// The recording, in a temporary file outside the root, until the run copies it.
    pub recording: Option<tempfile::TempPath>,
    pub end: End,
    pub quirks: QuirkLog,
    /// A harness error that ended the session.
    pub error: Option<RunError>,
}

/// The arguments, environment, and terminal of a session: the only place they are decided.
fn spawn_spec(
    context: &Context<'_>,
    scratch: &Scratch,
    profile: Profile,
    recording: Option<&Path>,
) -> SpawnSpec {
    SpawnSpec {
        program: context.binary.to_path_buf(),
        // The root, and nothing else: no flag, above all not `--disable-delete-confirmation`.
        args: vec![context.root.path().as_os_str().to_owned()],
        env: isolated_env(scratch, profile, true, None),
        cwd: scratch.cwd(),
        cols: TERMINAL.0,
        rows: TERMINAL.1,
        drain_bytes_per_sec: None,
        recording: recording.map(Path::to_path_buf),
        title: Some(format!("soak ({profile})")),
    }
}

/// Runs one session.
///
/// The program is always ended before this returns: a phase that passes its bound, an interrupt,
/// and the end of the run all kill its whole process group, and dropping the session does too.
///
/// # Errors
///
/// Returns [`SoakError`] when the session cannot be set up: no scratch area, no recording file,
/// no pseudo-terminal, or a program that cannot be started, and [`SoakError::NotStarted`] when the
/// person asked the soak to stop in the moment before the program would have started (nothing was
/// started). A harness error *during* the session is in the returned [`TuiRun`], so that what the
/// session measured is not lost.
pub(super) fn run_session(
    context: &Context<'_>,
    round: u32,
    profile: Profile,
) -> Result<TuiRun, SoakError> {
    let scope = format!("round {round}, tui {profile}");
    let scratch = Scratch::create(context.work_dir)?;
    let recording = if context.record {
        let file = tempfile::Builder::new()
            .prefix("xh-cast-")
            .suffix(".cast")
            .tempfile_in(context.work_dir)
            .map_err(|source| SoakError::Io {
                context: "cannot create the recording file",
                source,
            })?;
        Some(file.into_temp_path())
    } else {
        None
    };
    // The program is one that the soak has to end, and the guard that says so is taken before it
    // is started: refused when the person has already asked the soak to stop, and nothing starts
    // after that. The driver holds it until the program has been ended and waited for.
    let supervision = context.interrupt.supervising()?;
    let session = PtySession::spawn(&spawn_spec(
        context,
        &scratch,
        profile,
        recording.as_deref(),
    ))?;
    let live = Live::new(session, scratch.events());
    let mut driver = Driver::new(live, scratch.store(), context, scope);
    driver.supervision = Some(supervision);
    let result = driver.play();
    Ok(driver.conclude(result, &scratch, recording, round, profile))
}

/// What the program said when its scan ended: the `scan_complete` event.
#[derive(Debug, Clone, Copy)]
struct ScanEnd {
    /// Where the event is in the event log, so that the frames drawn after it can be told from
    /// the frames before it.
    event: usize,
    /// The number of entries it counts.
    entries: u64,
}

/// A session being driven: the program, what has been measured of it, and the quirks seen.
#[allow(
    clippy::struct_excessive_bools,
    reason = "what a session has done, each a fact of its own: it was probing, it confirmed the \
              quit, it opened a folder, and its program was killed for an interrupt"
)]
struct Driver<'a> {
    live: Live,
    recorder: Recorder,
    scope: String,
    limits: &'a super::Limits,
    run_deadline: Instant,
    interrupt: &'a super::Interrupt,
    /// Samples the scan store's size on a thread of its own, so that no walk of it delays what
    /// this thread does with the program's events.
    store_watch: StoreWatch,
    /// Whether the keys sent now are probes, which the recorder times.
    probing: bool,
    /// How many events [`Driver::observe`] has looked at.
    seen_events: usize,
    /// The `scan_complete` event, once it has come.
    scan_complete: Option<ScanEnd>,
    /// When the soak read a header with a badge that says the scan of what is shown has ended
    /// (neither `SCANNING` nor `REBUILDING`), on a screen that shows a frame the program drew after
    /// it said that the scan was over.
    complete_at: Option<Instant>,
    /// The badge it read then: `COMPLETE`, or the label of an uncertain result.
    ended_as: Option<HeaderState>,
    /// The dialog the last look at the screen found, so that one that stays on the screen is
    /// noted once.
    last_dialog: Option<String>,
    /// Whether the soak confirmed the quit.
    quit_confirmed: bool,
    /// Whether the soak opened a folder and came back up.
    drilled: bool,
    /// Whether the person's interrupt was seen while the program was running, and the program's
    /// process group was killed for it. From then on the session is over and is an interrupted
    /// one, whatever the wait that was in progress found: the kill can come in the very read that
    /// delivers what the wait was for, and nothing after it may go on as if the program were there.
    killed_for_interrupt: bool,
    /// The guard that tells the interrupt that the program is the soak's to end, held until
    /// [`Driver::end_the_program`] has ended it and waited for it, or given up on it, and dropped
    /// there. `None` for a driver whose program is nobody's to end (a test's).
    supervision: Option<Supervision>,
    /// What looks at the scratch area once the program is gone: [`look_at_residue`], and a stand-in
    /// in a test that asks what the interrupt held when the look was made.
    look_at_residue: fn(Instant, &super::Interrupt, &Scratch) -> Result<ScanResidue, ScratchError>,
    quirks: QuirkLog,
}

impl<'a> Driver<'a> {
    fn new(mut live: Live, store_dir: PathBuf, context: &Context<'a>, scope: String) -> Self {
        // Every read of a frame is made of the screen as the frame's mark left it, never of the
        // screen model that holds the first output of the next redraw as well.
        live.session.keep_the_screen_of_each_mark();
        let started = live.session.started();
        Self {
            recorder: Recorder::new(started),
            live,
            scope,
            limits: context.limits,
            run_deadline: context.run_deadline,
            interrupt: context.interrupt,
            store_watch: StoreWatch::start(store_dir, STORE_SAMPLE_EVERY),
            probing: false,
            seen_events: 0,
            scan_complete: None,
            complete_at: None,
            ended_as: None,
            last_dialog: None,
            quit_confirmed: false,
            drilled: false,
            killed_for_interrupt: false,
            supervision: None,
            look_at_residue,
            quirks: QuirkLog::new(),
        }
    }

    /// The screen as the latest frame's mark left it ([`PtySession::marked_screen`]): what every
    /// read of a frame is made of, the header, the inspector, and the dialogs. The read of a
    /// terminal can end in the middle of the redraw of the next frame, and the screen model
    /// then holds half of it.
    fn shown(&self) -> &Screen {
        self.live.session.marked_screen()
    }

    /// The write that every key passes, and the only one in the soak: the allowlist
    /// ([`Key::from_bytes`]) decides first; then the harness's own reader of what the program can
    /// read of the bytes written to it ([`Live::reading_of`]) must not say that they continue an
    /// escape sequence or ask for a deletion; then a deletion dialog on the screen stops the two
    /// keys that confirm one. Only then is the key written, and counted.
    fn vet(&self, bytes: &[u8]) -> Result<Key, Refusal> {
        let key = Key::from_bytes(bytes)?;
        if self.live.reading_of(key.bytes()).request {
            return Err(Refusal::Composes);
        }
        // A dialog that the screen model holds as well as one that a frame left stops the two keys
        // that confirm one: the model may hold a dialog that is being drawn.
        if (key == Key::ENTER || key == Key::Y)
            && (deletion_dialog_visible(self.live.session.screen())
                || deletion_dialog_visible(self.shown()))
        {
            return Err(Refusal::DeletionDialog);
        }
        Ok(key)
    }

    /// Sends `key` through the choke point.
    fn press(&mut self, key: Key) -> Result<Instant, RunError> {
        self.send_input(key.bytes())
    }

    /// Sends `key` through the choke point. `None` when the choke point refused it because the
    /// screen shows text that reads as a deletion dialog ([`Refusal::DeletionDialog`]): that is no
    /// error of the harness, since a name in the tree can read like that. The caller says so and
    /// goes on without the key. Any other refusal, and any other error, is an error.
    fn press_unless_refused_for_dialog_text(
        &mut self,
        key: Key,
    ) -> Result<Option<Instant>, RunError> {
        match self.press(key) {
            Ok(at) => Ok(Some(at)),
            Err(error) if refused_for_dialog_text(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Notes that a key was not sent because the screen shows text that reads as a deletion dialog.
    fn note_dialog_text(&mut self, what: &str) {
        let scope = self.scope.clone();
        self.quirks.note(
            QuirkKind::DialogText,
            &scope,
            format!(
                "the screen shows text that reads as a deletion dialog: {what}. The soak asks for \
                 no deletion, so it is a name in the tree, or the program is not the one under test"
            ),
        );
    }

    /// Looks at what the program has written since the last look: the events. The screen is not
    /// read here, because a read of the terminal can end in the middle of a frame: what the header
    /// says is read from a frame that the screen shows ([`Driver::await_complete`]).
    fn observe(&mut self) {
        let events = self.live.events.events();
        for (index, event) in events.iter().enumerate().skip(self.seen_events) {
            if let Payload::ScanComplete { entries } = event.payload {
                self.scan_complete.get_or_insert(ScanEnd {
                    event: index,
                    entries,
                });
            }
        }
        self.seen_events = events.len();
    }

    /// Whether the program says that the scan is over (the event). The header shows it a frame
    /// later, which is what [`Driver::await_complete`] waits for.
    const fn scan_is_over(&self) -> bool {
        self.scan_complete.is_some()
    }

    /// `limit` from now, but never past the end of the run. A limit too long for the clock to
    /// count is the end of the run.
    fn deadline(&self, limit: Duration) -> Instant {
        Instant::now()
            .checked_add(limit)
            .map_or(self.run_deadline, |at| at.min(self.run_deadline))
    }

    /// `stop`, unless the person ended the run, or the clock of the whole run did, which is the
    /// better account of why a wait did not end well.
    fn ended_by(&self, stop: Stop) -> Stop {
        if self.interrupt.is_set() {
            Stop::Interrupted
        } else if Instant::now() >= self.run_deadline {
            Stop::RunBound
        } else {
            stop
        }
    }

    /// Waits for `probe` to hold, before `deadline` and the end of the run, as one phase.
    fn wait_before<T>(
        &mut self,
        phase: SoakPhase,
        deadline: Instant,
        probe: impl FnMut(&Self) -> Option<T>,
    ) -> Result<T, Stop> {
        let waited = self.wait_until(deadline.min(self.run_deadline), probe)?;
        self.end_of_wait(waited, phase)
    }

    fn end_of_wait<T>(&self, waited: Waited<T>, phase: SoakPhase) -> Result<T, Stop> {
        match waited {
            Waited::Ready(value) => Ok(value),
            Waited::TimedOut => Err(self.ended_by(Stop::Timeout(phase))),
            Waited::Exited => Err(self.ended_by(Stop::Exited(phase))),
        }
    }

    /// Notes the dialog that the screen of the latest frame shows ([`Driver::shown`]). Nothing the
    /// soak does opens a dialog but the quit prompt, so any other is a quirk, and a deletion
    /// dialog is one that should never be seen. A dialog that is still there at the next look is
    /// the same one, and is noted once.
    fn look(&mut self, when: &str) {
        let found = dialog_on(self.shown(), when);
        self.note_dialog(found);
    }

    /// [`Driver::look`] at the screen model itself, which holds everything the terminal has been
    /// sent: for when no frame is to be expected, a key that drew none or a phase that passed its
    /// bound, and the dialog that the terminal shows is what there is to say why.
    fn look_at_the_terminal(&mut self, when: &str) {
        let found = dialog_on(self.live.session.screen(), when);
        self.note_dialog(found);
    }

    /// Notes the dialog that a look found: its identity, so that one that stays is noted once, and
    /// what to say of it.
    fn note_dialog(&mut self, found: Option<(QuirkKind, String, String)>) {
        let Some((kind, which, detail)) = found else {
            self.last_dialog = None;
            return;
        };
        if self.last_dialog.as_deref() == Some(which.as_str()) {
            return;
        }
        self.last_dialog = Some(which);
        let scope = self.scope.clone();
        self.quirks.note(kind, &scope, detail);
    }

    // ---------------------------------------------------------------------------------------
    // The phases.

    fn play(&mut self) -> Result<(), Stop> {
        self.first_frame()?;
        // One bound for the scan, counted from the first frame: the keys sent while it runs and
        // the wait for its end share it, so that the keys cannot postpone it.
        let scan_deadline = self.deadline(self.limits.scan);
        self.keys_during_the_scan(scan_deadline)?;
        self.await_complete(scan_deadline)?;
        self.drill()?;
        self.quit()
    }

    /// Waits for the first frame, and until the screen shows it. The event channel and the
    /// terminal are two routes, so the event can come before the bytes of the frame, and a look at
    /// the screen, or a key sent over what it shows, would be made on a screen that does not hold
    /// the frame yet: an error dialog in it could be dismissed by the first probe before anything
    /// had seen it.
    fn first_frame(&mut self) -> Result<(), Stop> {
        let deadline = self.deadline(self.limits.first_frame);
        let seq = self.wait_before(SoakPhase::FirstFrame, deadline, |driver| {
            driver.live.latest_frame_seq()
        })?;
        let shown = self.catch_up(seq, deadline)?;
        self.end_of_wait(shown, SoakPhase::FirstFrame)?;
        if !self.live.frame_marks() {
            let scope = self.scope.clone();
            self.quirks.note(
                QuirkKind::Protocol,
                &scope,
                "the program does not mark its frames in the terminal output, so the screen is \
                 not tied to a frame and screen reads are approximate",
            );
        }
        Ok(())
    }

    /// Sends `Esc` while the scan runs, each when the one before has been answered by a frame, and
    /// never past `scan_deadline`.
    fn keys_during_the_scan(&mut self, scan_deadline: Instant) -> Result<(), Stop> {
        self.probing = true;
        let result = self.probe_until_the_scan_is_over(scan_deadline);
        self.probing = false;
        result
    }

    fn probe_until_the_scan_is_over(&mut self, scan_deadline: Instant) -> Result<(), Stop> {
        let mut sent = 0;
        while sent < MAX_PROBES && !self.scan_is_over() {
            if sent > 0 {
                // Spread the probes over the scan: the gap, or the end of the scan, whichever
                // comes first.
                let until = self.deadline(PROBE_EVERY).min(scan_deadline);
                match self.wait_until(until, |driver| driver.scan_is_over().then_some(()))? {
                    Waited::Ready(()) => break,
                    Waited::TimedOut => {}
                    Waited::Exited => return Err(self.ended_by(Stop::Exited(SoakPhase::Scan))),
                }
                if let Some(stop) = self.scan_must_stop(scan_deadline) {
                    return Err(stop);
                }
            }
            // An `Esc` dismisses an error dialog, so the screen is looked at before it is sent,
            // the first one included: a dialog that came up since the last key would be gone from
            // the frame this one draws.
            self.look("during the scan");
            self.press(Key::ESC)?;
            sent += 1;
            let until = self.deadline(self.limits.key).min(scan_deadline);
            match self.settle_until(until)? {
                Waited::Ready(()) => {}
                Waited::TimedOut => {
                    if let Some(stop) = self.scan_must_stop(scan_deadline) {
                        return Err(stop);
                    }
                    // A dialog that does not answer `Esc` draws no frame for it, and what the
                    // screen shows is the only account of why.
                    self.look_at_the_terminal("when a key drew no frame");
                    // A program that does not answer one key will not answer the next: say so
                    // once, and let the scan run.
                    let scope = self.scope.clone();
                    self.quirks.note(
                        QuirkKind::NoFrameForKey,
                        &scope,
                        format!(
                            "no frame counted a key within {} ms; no more keys are sent during \
                             the scan",
                            self.limits.key.as_millis()
                        ),
                    );
                    return Ok(());
                }
                Waited::Exited => return Err(self.ended_by(Stop::Exited(SoakPhase::Scan))),
            }
            self.look("during the scan");
        }
        Ok(())
    }

    /// The stop that the person, the clock of the whole run, or the bound on the scan asks for,
    /// if any does.
    fn scan_must_stop(&self, scan_deadline: Instant) -> Option<Stop> {
        self.stop_of_the_run()
            .or_else(|| (Instant::now() >= scan_deadline).then_some(Stop::Timeout(SoakPhase::Scan)))
    }

    /// The stop that the person or the clock of the whole run asks for, if either does.
    fn stop_of_the_run(&self) -> Option<Stop> {
        if self.interrupt.is_set() {
            Some(Stop::Interrupted)
        } else if Instant::now() >= self.run_deadline {
            Some(Stop::RunBound)
        } else {
            None
        }
    }

    /// Waits until the scan is over and the screen shows it, within `scan_deadline`: the
    /// `scan_complete` event, and then a frame, drawn after the event, that the screen shows and
    /// whose header badge says that the scan of what is shown has ended
    /// ([`HeaderState::is_finished`]).
    ///
    /// The header is read only from a screen that shows a frame (the mark of the frame has been
    /// read: [`Driver::wait_for_a_shown_frame`]) and never in the middle of one. A read of the
    /// terminal can end half way through the redraw of the badge, and half of the label of a scan
    /// that ends `COMPLETE` is not the label of an uncertain one.
    fn await_complete(&mut self, scan_deadline: Instant) -> Result<(), Stop> {
        let event = self.wait_before(SoakPhase::Complete, scan_deadline, |driver| {
            driver.scan_complete.map(|end| end.event)
        })?;
        let state = self.wait_for_a_shown_frame(
            SoakPhase::Complete,
            scan_deadline,
            Some(event),
            None,
            |screen| header_state(screen).filter(HeaderState::is_finished),
        )?;
        self.complete_at = Some(Instant::now());
        self.ended_as = Some(state);
        self.look("when the scan completed");
        self.note_how_the_scan_ended();
        Ok(())
    }

    /// The latest frame the program has reported after the event at index `after` (any frame when
    /// that is `None`), if it is a later frame than `since`.
    fn latest_frame_since(&self, after: Option<usize>, since: Option<u64>) -> Option<u64> {
        let events = self.live.events.events();
        let from = after.map_or(0, |index| index + 1);
        let seq = events
            .get(from..)?
            .iter()
            .rev()
            .find_map(|event| match event.payload {
                Payload::Frame { seq, .. } => Some(seq),
                _ => None,
            })?;
        since.is_none_or(|since| seq > since).then_some(seq)
    }

    /// Waits until the screen shows a frame that satisfies `accept`, before `deadline`. Each frame
    /// the program draws after the event at index `after` (any, when that is `None`) and later
    /// than `since` is waited for in turn: the screen is caught up to it ([`Drive::catch_up`]: the
    /// mark of the frame has been read), and only then is `accept` asked about the screen as the
    /// latest mark left it ([`Driver::shown`]), which is the screen of a frame and not of a
    /// redraw in progress: the read that brought the mark can have brought the first output of
    /// the next frame as well, and the screen model holds that. A frame that does not satisfy it
    /// is followed by the next one. The frame waited for is always the latest at the time, so a
    /// program that keeps drawing cannot keep the wait from ending.
    fn wait_for_a_shown_frame<T>(
        &mut self,
        phase: SoakPhase,
        deadline: Instant,
        after: Option<usize>,
        mut since: Option<u64>,
        mut accept: impl FnMut(&Screen) -> Option<T>,
    ) -> Result<T, Stop> {
        loop {
            let seq = self.wait_before(phase, deadline, |driver| {
                driver.latest_frame_since(after, since)
            })?;
            let shown = self.catch_up(seq, deadline)?;
            self.end_of_wait(shown, phase)?;
            if let Some(found) = accept(self.shown()) {
                return Ok(found);
            }
            since = Some(seq);
        }
    }

    /// Notes a scan that did not end `COMPLETE`: its result is uncertain, as the label says.
    /// Counted by kind in `summary.json`; the label is text, and goes to `quirks.txt` alone.
    fn note_how_the_scan_ended(&mut self) {
        let Some(state) = self
            .ended_as
            .clone()
            .filter(|state| *state != HeaderState::Complete)
        else {
            return;
        };
        let scope = self.scope.clone();
        self.quirks.note(
            QuirkKind::UncertainScan,
            &scope,
            format!(
                "the scan ended with the header `{}`: its result is uncertain",
                state.label()
            ),
        );
    }

    /// The entry the inspector shows on the screen of the latest frame, if it shows one.
    fn selected(&self) -> Option<SelectedItem> {
        match inspector(self.shown()) {
            Inspector::Item(item) => Some(item),
            Inspector::NotShown | Inspector::NothingSelected => None,
        }
    }

    /// Opens a folder and goes back up. At the end of a scan nobody has moved the cursor, so it
    /// is on the largest entry; when that is a file, the cursor is first walked to a folder
    /// ([`Driver::walk_to_a_folder`]). Where the screen is exact the inspector says whether the
    /// entry under the cursor is a folder; where it is not, `Enter` is sent anyway, and what the
    /// screen shows afterwards is a measurement and not a condition.
    ///
    /// That the folder is open, and that the root is back, is read from the screen of a frame
    /// ([`Driver::wait_for_a_shown_frame`]), never from a redraw in progress, and each time is
    /// taken only once the frame has been read. The folder is told from the root by the path in
    /// the header. Where the header has cut the root's path in the middle (see
    /// [`header_is_cut`]) that text is not an identity, since two paths can be cut to the same
    /// text, and the inspector says which view it is instead: what is open is the folder that was
    /// under the cursor, so the pane shows another entry (or none, for a folder with nothing in
    /// it), and going back up puts the cursor on the folder again. An entry is told by its whole
    /// pane ([`SelectedItem::pane`]) and not by its name, which is cut to fit and which a folder
    /// can share with what is in it. Where the screen is not exact the inspector is not read, and
    /// a root with a cut path is not drilled.
    ///
    /// The walk to a folder and the wait for it to show share one bound ([`Limits::drill`]),
    /// which is what the phase times out as when it passes.
    fn drill(&mut self) -> Result<(), Stop> {
        let exact = self.live.screen_is_exact();
        let Some(root_header) = header_path(self.shown()) else {
            return Ok(());
        };
        let by_entry = header_is_cut(&root_header);
        if by_entry && !exact {
            let scope = self.scope.clone();
            self.quirks.note(
                QuirkKind::DrillSkipped,
                &scope,
                "no folder was opened: the header cuts the root's path in the middle, two paths \
                 that the program cuts can read the same, and on a terminal whose screen is not \
                 exact the soak does not read the inspector that would tell the folder from the \
                 root",
            );
            return Ok(());
        }
        let drill_deadline = self.deadline(if exact {
            self.limits.drill
        } else {
            INEXACT_DRILL_WAIT
        });
        let entered = if exact {
            match self.selected() {
                Some(item) if item.kind == "folder" => Some(item.pane),
                Some(item) => match self.walk_to_a_folder(&item, drill_deadline)? {
                    Some(folder) => Some(folder),
                    None => return Ok(()),
                },
                None => return Ok(()),
            }
        } else {
            None
        };
        let finished = |screen: &Screen| header_state(screen).is_some_and(|s| s.is_finished());
        let folder_is_shown = |screen: &Screen| {
            entered
                .as_deref()
                .is_some_and(|folder| shows_the_inside_of(screen, folder))
        };
        let folder_is_selected = |screen: &Screen| {
            entered
                .as_deref()
                .is_some_and(|folder| shows_the_cursor_on(screen, folder))
        };

        let since = self.live.latest_frame_seq();
        let Some(at) = self.press_unless_refused_for_dialog_text(Key::ENTER)? else {
            self.note_dialog_text("`Enter` was not sent, so no folder was opened");
            return Ok(());
        };
        self.recorder.start_measure("drill_ms", at);
        let below =
            self.wait_for_a_shown_frame(SoakPhase::Drill, drill_deadline, None, since, |screen| {
                let moved = header_path(screen).is_some_and(|path| path != root_header)
                    || (by_entry && folder_is_shown(screen));
                (moved && finished(screen)).then_some(())
            });
        match below {
            Ok(()) => {
                self.recorder.stop_measure("drill_ms", Instant::now());
            }
            Err(Stop::Timeout(SoakPhase::Drill)) if !exact => return Ok(()),
            Err(stop) => return Err(stop),
        }
        self.look("in the folder");

        let since = self.live.latest_frame_seq();
        let at = self.press(Key::ESC)?;
        self.recorder.start_measure("up_ms", at);
        let up_deadline = self.deadline(self.limits.drill);
        self.wait_for_a_shown_frame(SoakPhase::Up, up_deadline, None, since, |screen| {
            let back = header_path(screen).is_some_and(|path| path == root_header)
                && (!by_entry || folder_is_selected(screen));
            (back && finished(screen)).then_some(())
        })?;
        self.recorder.stop_measure("up_ms", Instant::now());
        self.drilled = true;
        self.look("back at the root");
        Ok(())
    }

    /// Moves the cursor from the file `start` to a folder with the arrow keys, before `deadline`,
    /// and returns the folder's pane (what tells it from every other entry, see
    /// [`SelectedItem::pane`]), or `None` when the walk found none. The map is a treemap, so
    /// there is no list to walk down. The walk ([`Walk`]) presses a key, waits for the frame that
    /// draws it, and reads the inspector: a folder ends it, and any other entry is noted along
    /// with the key that led to it. Entries are told apart by their panes, not their names: a
    /// name is cut to fit the pane, and two entries can show one name. The keys are tried from
    /// every entry the cursor reaches, and the walk goes back along the keys it knows to an entry
    /// that still has one to try. Only arrow keys are sent, and each through the choke point.
    ///
    /// The walk ends without a quirk when it has been all around the map the cursor can reach and
    /// found no folder, which is how a root of files alone ends. It ends with one
    /// ([`QuirkKind::DrillSkipped`]) when it was cut short: it used its [`MAX_WALK`] keys, a key
    /// drew no frame, the inspector went blank, or the cursor cannot get back to an entry that
    /// has keys left, so that the map cannot be said to have been walked all around.
    fn walk_to_a_folder(
        &mut self,
        start: &SelectedItem,
        deadline: Instant,
    ) -> Result<Option<String>, Stop> {
        let mut walk = Walk::new(&start.pane);
        let mut pressed = 0;
        loop {
            let arrow = match walk.next() {
                Next::Press(arrow) if pressed < MAX_WALK => arrow,
                Next::Press(_) => {
                    self.cut_the_walk_short("it used all its keys", walk.entries());
                    return Ok(None);
                }
                Next::Done => return Ok(None),
                Next::Stuck => {
                    self.cut_the_walk_short(
                        "the cursor cannot get back to an entry that has keys left",
                        walk.entries(),
                    );
                    return Ok(None);
                }
            };
            self.press(arrow.key())?;
            pressed += 1;
            let until = self.deadline(self.limits.key).min(deadline);
            match self.settle_until(until)? {
                Waited::Ready(()) => {}
                Waited::TimedOut => {
                    if let Some(stop) = self.stop_of_the_run() {
                        return Err(stop);
                    }
                    if Instant::now() >= deadline {
                        return Err(Stop::Timeout(SoakPhase::Drill));
                    }
                    self.look_at_the_terminal("when a key drew no frame");
                    let scope = self.scope.clone();
                    self.quirks.note(
                        QuirkKind::NoFrameForKey,
                        &scope,
                        format!(
                            "no frame counted an arrow key within {} ms while the cursor was \
                             moved to a folder",
                            self.limits.key.as_millis()
                        ),
                    );
                    self.cut_the_walk_short("a key drew no frame", walk.entries());
                    return Ok(None);
                }
                Waited::Exited => return Err(self.ended_by(Stop::Exited(SoakPhase::Drill))),
            }
            self.look("while the cursor was moved to a folder");
            let Some(item) = self.selected() else {
                self.cut_the_walk_short("the inspector went blank", walk.entries());
                return Ok(None);
            };
            if item.kind == "folder" {
                return Ok(Some(item.pane));
            }
            walk.observe(arrow, &item.pane);
        }
    }

    /// Notes that the walk to a folder was cut short.
    fn cut_the_walk_short(&mut self, why: &str, seen: usize) {
        let scope = self.scope.clone();
        self.quirks.note(
            QuirkKind::DrillSkipped,
            &scope,
            format!(
                "no folder was opened: the cursor started on a file, and the walk to a folder \
                 with the arrow keys was cut short ({why}) after {seen} entries, with at most \
                 {MAX_WALK} keys"
            ),
        );
    }

    /// The shared quit path: `q`, the frame that counts it and the prompt, and `y` when the prompt
    /// offers the plain quit. The prompt and the exit share one bound: `Limits::quit` is how long
    /// the whole quit may take.
    fn quit(&mut self) -> Result<(), Stop> {
        let until = self.deadline(self.limits.quit);
        let view = match self.request_quit(until) {
            Ok(view) => view,
            Err(ProtocolError::Run(error)) => return Err(Stop::Error(error)),
            Err(ProtocolError::Unmet(unmet)) => {
                let scope = self.scope.clone();
                self.quirks.note(
                    QuirkKind::QuitRefused,
                    &scope,
                    format!(
                        "the quit prompt did not open: expected {}; {}",
                        unmet.expected, unmet.observed
                    ),
                );
                return Err(match unmet.waited {
                    Waited::TimedOut => self.ended_by(Stop::Timeout(SoakPhase::Quit)),
                    Waited::Exited => self.ended_by(Stop::Exited(SoakPhase::Quit)),
                    Waited::Ready(()) => Stop::QuitRefused,
                });
            }
        };
        if !offers_plain_quit(&view) {
            let scope = self.scope.clone();
            self.quirks.note(
                QuirkKind::QuitRefused,
                &scope,
                format!(
                    "the quit prompt does not offer the plain quit: {}",
                    view.text
                ),
            );
            return Err(Stop::QuitRefused);
        }
        let Some(at) = self.press_unless_refused_for_dialog_text(Key::Y)? else {
            self.note_dialog_text(
                "`y` was not sent, so the quit was not confirmed, and the program was killed \
                 instead",
            );
            return Err(Stop::QuitNotConfirmed);
        };
        self.quit_confirmed = true;
        self.recorder.record_quit_confirmed(at);
        self.wait_before(SoakPhase::Exit, until, |driver| {
            driver.live.session.exit().map(|_| ())
        })?;
        Ok(())
    }

    // ---------------------------------------------------------------------------------------
    // The end.

    /// Ends the program if it is still there, reads what is left of its output, and says what the
    /// session found. The program's process group is killed when it has not ended by itself.
    fn conclude(
        mut self,
        result: Result<(), Stop>,
        scratch: &Scratch,
        recording: Option<tempfile::TempPath>,
        round: u32,
        profile: Profile,
    ) -> TuiRun {
        // A program that was killed for the person's interrupt ends the session as an interrupted
        // one, whatever the wait that saw the kill found: it can be the very wait that the same
        // read answered.
        let result = if self.killed_for_interrupt {
            Err(Stop::Interrupted)
        } else {
            result
        };
        let (killed, cleanup_error) = self.end_the_program();
        let exit = self.live.session.exit();
        let modes = self.live.session.modes();
        // What the program left behind. A scratch area that cannot be read is a harness error and
        // not residue: no entry is made up for what could not be listed, and the count stays at
        // what was seen. The program is not held any more: this is file system work.
        let (residue, residue_error) =
            match (self.look_at_residue)(self.run_deadline, self.interrupt, scratch) {
                Ok(residue) => (residue, None),
                Err(error) => (ScanResidue::default(), Some(RunError::from(error))),
            };
        let mut stopped = self.note_the_stop(result.err(), exit);
        // The program is dead, its output read, its recording flushed, and its scratch area looked
        // at, as far as each could be. An error in doing any of it means that what was recorded is
        // not what happened: a harness error, and it ends the run as one.
        let harness_errors = [
            ("ending the session", cleanup_error),
            (
                "reading what the program left in its scratch area",
                residue_error,
            ),
        ];
        for (what, error) in harness_errors {
            let Some(error) = error else {
                continue;
            };
            let scope = self.scope.clone();
            self.quirks
                .note(QuirkKind::HarnessError, &scope, format!("{what}: {error}"));
            if stopped.error.is_none() {
                stopped.error = Some(error);
            }
        }
        if !killed {
            self.note_the_end(exit, modes, &residue);
        }
        let metrics = self.metrics();
        if let Some(stall) = metrics
            .get("max_stall_ms")
            .filter(|ms| **ms > STALL_QUIRK_MS)
        {
            let scope = self.scope.clone();
            self.quirks.note(
                QuirkKind::Stall,
                &scope,
                format!("{stall:.0} ms between two frames while the program was busy"),
            );
        }
        let via = if killed {
            ExitVia::Killed
        } else if self.quit_confirmed {
            ExitVia::Quit
        } else {
            ExitVia::Exited
        };
        TuiRun {
            round,
            profile,
            exit: SoakExit {
                code: exit.and_then(|exit| exit.code).map(i64::from),
                signal: exit.and_then(|exit| exit.signal).map(i64::from),
                via,
            },
            timed_out_phase: stopped.timed_out_phase,
            terminal_restored: modes.restored(),
            residue,
            drilled: self.drilled,
            metrics,
            recording,
            end: stopped.end,
            quirks: self.quirks,
            error: stopped.error,
        }
    }

    /// Kills the program's process group if the program is still there, then reads what is left
    /// of its output and closes the recording. Returns whether the program was killed (now, or
    /// earlier for the person's interrupt), and the first error that any of it met: every step is
    /// attempted whatever the one before it did, so that a failing flush does not leave the
    /// program running or its output unread. A program that is still there after the kill, which
    /// is a program stuck where a signal does not reach it, is an error: the session would
    /// otherwise be reported as ended and the next one started beside a program that still runs.
    ///
    /// The guard that says the program is the soak's to end is dropped as soon as the program has
    /// been ended and waited for, or given up on, and before anything else is read or written:
    /// the rest of its output, the recording, and later the scratch area are file system work
    /// that can block, and the exit is not armed while a program is held.
    fn end_the_program(&mut self) -> (bool, Option<RunError>) {
        let mut first: Option<RunError> = None;
        let mut remember = |result: Result<(), RunError>| {
            if let Err(error) = result
                && first.is_none()
            {
                first = Some(error);
            }
        };
        remember(self.read_the_program());
        let killed = if self.live.session.exit().is_none() {
            if !self.live.session.kill() {
                remember(Err(RunError::Io {
                    context: "the program could not be ended",
                    source: io::Error::other(
                        "its process group was killed and the program was still there after the \
                         time that the kill is given, so it may still be running",
                    ),
                }));
            }
            true
        } else {
            self.killed_for_interrupt
        };
        self.supervision = None;
        remember(
            self.live
                .session
                .drain(DRAIN_QUIET, DRAIN_LIMIT)
                .map_err(RunError::from),
        );
        remember(self.read_the_program());
        remember(self.live.session.finish_recording().map_err(RunError::from));
        (killed, first)
    }

    /// Reads what the program has written, its output and its events, and what the recorder keeps
    /// of them. Never ends the program: [`Drive::pump`] does that for an interrupt, and this is
    /// what the end of a session reads with once it has ended the program itself.
    fn read_the_program(&mut self) -> Result<(), RunError> {
        self.live.session.pump()?;
        self.live.poll_events()?;
        self.observe();
        if let Some(exit) = self.live.session.exit() {
            self.recorder.record_exit(exit.at);
        }
        Ok(())
    }

    /// Notes why the session ended early, if it did, and says whether the run goes on.
    fn note_the_stop(&mut self, stop: Option<Stop>, exit: Option<ExitInfo>) -> Stopped {
        let scope = self.scope.clone();
        let mut stopped = Stopped {
            end: End::Session,
            timed_out_phase: None,
            error: None,
        };
        match stop {
            None | Some(Stop::QuitRefused | Stop::QuitNotConfirmed) => {}
            Some(Stop::Timeout(phase)) => {
                stopped.timed_out_phase = Some(phase);
                // What the screen shows when a bound passes is the account of it when a dialog
                // is what held the program up. The quit prompt is the one dialog that is expected.
                if !matches!(phase, SoakPhase::Quit | SoakPhase::Exit) {
                    self.look_at_the_terminal("when the phase passed its bound");
                }
                self.quirks.note(
                    QuirkKind::Timeout,
                    &scope,
                    format!(
                        "the {phase} phase passed its bound; the program's process group was \
                         killed"
                    ),
                );
            }
            Some(Stop::Exited(phase)) => {
                let ended = exit.map_or_else(|| "it is still running".to_owned(), |e| e.describe());
                self.quirks.note(
                    QuirkKind::ExitedEarly,
                    &scope,
                    format!(
                        "the program ended during the {phase} phase ({ended}); the screen \
                         showed: {}",
                        self.live.session.screen().text()
                    ),
                );
            }
            Some(Stop::RunBound) => stopped.end = End::RunBound,
            Some(Stop::Interrupted) => stopped.end = End::Interrupted,
            Some(Stop::Error(error)) => {
                self.quirks
                    .note(QuirkKind::HarnessError, &scope, error.to_string());
                stopped.error = Some(error);
            }
        }
        stopped
    }

    /// Notes what is odd about a program that ended by itself, or was quit: a status other than
    /// 0, a terminal left in a mode, files left behind. A program the soak had to kill is not
    /// held to any of it.
    fn note_the_end(
        &mut self,
        exit: Option<ExitInfo>,
        modes: TerminalModes,
        residue: &ScanResidue,
    ) {
        let scope = self.scope.clone();
        if let Some(exit) = exit.filter(|exit| exit.code != Some(0)) {
            self.quirks.note(
                QuirkKind::NonZeroExit,
                &scope,
                format!("the program ended with {}", exit.describe()),
            );
        }
        if !modes.restored() {
            self.quirks.note(
                QuirkKind::TerminalNotRestored,
                &scope,
                format!("the terminal was left in {modes:?}"),
            );
        }
        if !residue.is_empty() {
            self.quirks
                .note(QuirkKind::Residue, &scope, residue.describe_for("program"));
        }
    }

    /// The named measurements of the session so far.
    fn metrics(&mut self) -> BTreeMap<String, f64> {
        let started = self.live.session.started();
        if let Some(at) = self.complete_at {
            self.recorder.record_metric(
                "complete_ms",
                crate::metrics::milliseconds(at.saturating_duration_since(started)),
            );
        }
        if let Some(ScanEnd { entries, .. }) = self.scan_complete {
            #[allow(clippy::cast_precision_loss, reason = "an entry count far below 2^52")]
            self.recorder
                .record_metric("scan_complete_entries", entries as f64);
        }
        let session = &self.live.session;
        let now = Instant::now();
        self.recorder.record_metric(
            "duration_ms",
            crate::metrics::milliseconds(now.saturating_duration_since(started)),
        );
        self.recorder.finish(
            self.live.events.events(),
            session.output_bytes(),
            session.sampler(),
            self.store_watch.finish(),
            session.cpu_time(),
            now,
        )
    }
}

impl Drive for Driver<'_> {
    fn live(&self) -> &Live {
        &self.live
    }

    fn live_mut(&mut self) -> &mut Live {
        &mut self.live
    }

    /// Reads the program and, when the person has interrupted the run, kills the program's
    /// process group and says so with an error, which ends whatever wait was in progress before
    /// it can answer: a wait answers by reading first and asking its question after, so a kill
    /// that came in the same read as what the wait was for would otherwise be answered `Ready`,
    /// and the session would go on with a key to a program that is gone. A program that has
    /// ended by itself is not killed for the interrupt: nothing was cut short. The scan store is
    /// sampled on a thread of its own ([`StoreWatch`]), so nothing here waits for a walk of it.
    fn pump(&mut self) -> Result<(), RunError> {
        self.read_the_program()?;
        if self.interrupt.is_set() {
            if self.live.session.exit().is_none() {
                self.killed_for_interrupt = true;
                self.live.session.kill();
            }
            if self.killed_for_interrupt {
                return Err(RunError::Io {
                    context: "the person interrupted the run",
                    source: io::Error::new(
                        io::ErrorKind::Interrupted,
                        "the program's process group was killed",
                    ),
                });
            }
        }
        Ok(())
    }

    /// Every key is counted, so that `inputs_sent` is the total; only the probes are timed (the
    /// recorder times the inputs that were sent isolated, and the others are not).
    fn input_sent(&mut self, sent: &Sent) {
        self.recorder.record_input(
            sent.at,
            sent.number,
            sent.reflecting,
            sent.isolated && self.probing,
        );
    }

    /// The choke point (see the module documentation): the one place that writes a key to the
    /// program. Everything that sends a key, the shared protocols of `runner::live` included,
    /// calls this, and a write that is not exactly one allowed key is refused before anything is
    /// written. (The pseudo-terminal layer's answer to a cursor position request, and the close of
    /// the terminal when the session is dropped, are not keys and do not pass here.)
    fn send_input(&mut self, bytes: &[u8]) -> Result<Instant, RunError> {
        let key = self.vet(bytes).map_err(|refusal| RunError::Io {
            context: "the soak refuses to send a key",
            source: io::Error::new(io::ErrorKind::PermissionDenied, refusal),
        })?;
        let sent = match self.live.send_input(key.bytes()) {
            Ok(sent) => sent,
            Err(error) => {
                // A key is counted when it is attempted, not when its write is known to have
                // gone well: the write can have reached the program before it failed (the
                // terminal's write can fail part way, and the recording of a key is made after
                // the key is written). `inputs_sent` is the number of keys the program may have
                // been sent, and this one is not timed.
                self.recorder.record_input(
                    Instant::now(),
                    self.live.inputs_sent(),
                    self.live.reflecting_counter(),
                    false,
                );
                return Err(error.into());
            }
        };
        self.input_sent(&sent);
        Ok(sent.at)
    }

    /// The one protocol of `runner::live` that writes to the program around
    /// [`Driver::send_input`]: it asks the program to say that it has read everything, with a byte
    /// that is not a key. Nothing in a soak needs it (only a deletion that was requested outside
    /// the protocols does), so it is refused here, and the driver cannot write that byte at all.
    fn barrier(&mut self, _deadline: Instant) -> Result<Waited<()>, RunError> {
        Err(RunError::Io {
            context: "the soak refuses to write an input barrier",
            source: io::Error::new(io::ErrorKind::PermissionDenied, Refusal::Barrier),
        })
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::fs;

    use super::*;
    use crate::soak::{Interrupt, Limits, SoakRoot};

    const PATIENCE: Duration = Duration::from_secs(30);

    /// Starts a shell script, as the program of a session, in a pseudo-terminal, and hands the
    /// soak's own `Driver` over it to `body`, with the file the script appends everything it reads
    /// to. `script` says what the shell runs, given that file.
    fn with_program<R>(
        script: impl FnOnce(&Path) -> String,
        body: impl FnOnce(&mut Driver<'_>, &Path) -> R,
    ) -> R {
        let tree = tempfile::Builder::new()
            .prefix("xt-tree-")
            .tempdir()
            .expect("a tree");
        let work = tempfile::Builder::new()
            .prefix("xt-work-")
            .tempdir()
            .expect("a work directory");
        let root = SoakRoot::open(tree.path()).expect("a root");
        let limits = Limits::default();
        let interrupt = Interrupt::new();
        let context = Context {
            root: &root,
            binary: Path::new("/bin/sh"),
            work_dir: work.path(),
            limits: &limits,
            run_deadline: Instant::now() + PATIENCE * 4,
            interrupt: &interrupt,
            record: false,
        };
        let scratch = Scratch::create(work.path()).expect("a scratch area");
        let received = work.path().join("received");
        let session = PtySession::spawn(&SpawnSpec {
            program: PathBuf::from("/bin/sh"),
            args: vec!["-c".into(), script(&received).into()],
            env: isolated_env(&scratch, Profile::Default, true, None),
            cwd: scratch.cwd(),
            cols: TERMINAL.0,
            rows: TERMINAL.1,
            drain_bytes_per_sec: None,
            recording: None,
            title: None,
        })
        .expect("a program in a pseudo-terminal");
        let mut driver = Driver::new(
            Live::new(session, scratch.events()),
            scratch.store(),
            &context,
            "test".to_owned(),
        );
        let result = body(&mut driver, &received);
        driver.live.session.kill();
        result
    }

    /// Waits until the program has set the terminal raw and said so.
    fn ready(driver: &mut Driver<'_>) {
        let waited = driver
            .wait_until(Instant::now() + PATIENCE, |driver| {
                driver
                    .live
                    .session
                    .screen()
                    .text()
                    .contains("ready")
                    .then_some(())
            })
            .expect("the program is driven");
        assert!(
            matches!(waited, Waited::Ready(())),
            "the program never said it was ready"
        );
    }

    /// Waits until everything the program has read is exactly `expected`.
    fn received_exactly(driver: &mut Driver<'_>, received: &Path, expected: &[u8]) {
        let waited = driver
            .wait_until(Instant::now() + PATIENCE, |_| {
                (fs::read(received).ok()? == expected).then_some(())
            })
            .expect("the program is driven");
        assert!(
            matches!(waited, Waited::Ready(())),
            "the program did not read exactly {expected:02x?}: {:02x?}",
            fs::read(received).unwrap_or_default()
        );
    }

    #[test]
    fn a_write_that_is_not_one_allowed_key_is_refused_and_nothing_reaches_the_program() {
        with_program(
            |received| {
                format!(
                    "stty raw -echo; echo ready >&2; exec /bin/cat >> '{}'",
                    received.display()
                )
            },
            |driver, received| {
                ready(driver);
                driver.send_input(b"j").expect("an allowed key is written");
                received_exactly(driver, received, b"j");

                for refused in [
                    &[0x7f][..],
                    &[0x08],
                    &[0x1d],
                    b"\x1b\x7f",
                    b"\x1b[121u",
                    b"qy",
                    b"H",
                    b"/",
                    b"",
                ] {
                    let Err(RunError::Io { source, .. }) = driver.send_input(refused) else {
                        panic!("{refused:02x?} was not refused");
                    };
                    assert_eq!(
                        source.kind(),
                        io::ErrorKind::PermissionDenied,
                        "{refused:02x?}"
                    );
                }
                assert_eq!(
                    driver.live.inputs_sent(),
                    1,
                    "a refused write is not an input"
                );

                driver.send_input(b"k").expect("an allowed key is written");
                received_exactly(driver, received, b"jk");
                driver
                    .wait_quietly(Duration::from_millis(150))
                    .expect("the program is driven");
                assert_eq!(
                    fs::read(received).expect("what the program read"),
                    b"jk",
                    "the terminal keeps the order of writes, so a refused byte that was written \
                     would sit between the two keys, or arrive after them"
                );
            },
        );
    }

    #[test]
    fn the_input_barrier_is_refused_and_nothing_reaches_the_program() {
        // `Drive::barrier` is the one provided protocol that writes around `send_input`: a
        // driver that did not override it would send the byte `0x1d` to the program.
        with_program(
            |received| {
                format!(
                    "stty raw -echo; echo ready >&2; exec /bin/cat >> '{}'",
                    received.display()
                )
            },
            |driver, received| {
                ready(driver);

                let Err(RunError::Io { source, .. }) = driver.barrier(Instant::now() + PATIENCE)
                else {
                    panic!("a barrier was written");
                };

                assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
                assert!(source.to_string().contains("input barrier"), "{source}");
                driver.press(Key::J).expect("a key is not refused");
                received_exactly(driver, received, b"j");
                driver
                    .wait_quietly(Duration::from_millis(150))
                    .expect("the program is driven");
                assert_eq!(
                    fs::read(received).expect("what the program read"),
                    b"j",
                    "no barrier byte, before the key or after it"
                );
            },
        );
    }

    #[test]
    fn no_key_that_could_confirm_a_dialog_is_written_while_a_deletion_dialog_is_on_the_screen() {
        with_program(
            |received| {
                format!(
                    "stty raw -echo; printf '[Enter/y] start\\r\\n' >&2; echo ready >&2; \
                     exec /bin/cat >> '{}'",
                    received.display()
                )
            },
            |driver, received| {
                ready(driver);
                for key in [Key::ENTER, Key::Y] {
                    let Err(RunError::Io { source, .. }) = driver.press(key) else {
                        panic!("{key} was sent over a deletion dialog");
                    };
                    assert!(source.to_string().contains("deletion dialog"), "{source}");
                }
                driver.press(Key::J).expect("a movement key is not refused");
                received_exactly(driver, received, b"j");
            },
        );
    }

    // -----------------------------------------------------------------------------------------
    // Whole sessions, against shell scripts that stand in for `excise`.
    //
    // A script draws what a session reads (the header, the inspector pane, a dialog) the way
    // `excise` does, answers each key with a frame that counts it, and writes down every key it
    // read in the file `received`, so that a test can say what the program was sent, and that no
    // key was one that deletes.

    use std::os::unix::fs::PermissionsExt as _;

    use crate::runner::scripted::{boxed, place, prelude, screen};

    const SCANNING: &str = " EXCISE  /scripted/  ◌ SCANNING";
    const COMPLETE: &str = " EXCISE  /scripted/  ◆ COMPLETE";
    const NEEDS_REVIEW: &str = " EXCISE  /scripted/  ? NEEDS REVIEW";
    const INSIDE: &str = " EXCISE  /scripted/photos/  ◆ COMPLETE";
    /// The program says that its scan is over; the header says so in the frame that follows.
    const SCAN_COMPLETE: &str = "printf '{\"v\":1,\"kind\":\"scan_complete\",\"entries\":5,\"t_us\":2}\\n' >> \"$events\"\n";
    /// The program says that it built the quit prompt.
    const QUIT_PROMPT: &str =
        "printf '{\"v\":1,\"kind\":\"quit_prompt\",\"t_us\":3}\\n' >> \"$events\"\n";
    const ERROR: (&str, &[&str]) = ("ERROR", &["Could not read the folder", "[Esc] close"]);
    const QUIT: (&str, &[&str]) = (
        "QUIT",
        &["Quit Excise?", "", "[y] Quit", "[Esc/q/n] Keep working"],
    );

    /// What every script here starts with, after the shared prelude: the terminal raw; `log` to
    /// write a key down; `press CODE` to read one key, write it down, and end the program with
    /// status 9 if it is not the byte with that octal code; and `arrow CODE` to do the same for
    /// the three bytes of an arrow key (`ESC [` and the letter with that octal code).
    const SCRIPT_HEAD: &str = "stty raw -echo\n\
         log() { printf '%s\\n' \"$1\" >> \"@DIR@/received\"; }\n\
         press() { key; log \"$byte\"; [ \"$byte\" = \"$1\" ] || exit 9; }\n\
         arrow() { key; a=\"$byte\"; key; b=\"$byte\"; key; log \"$a $b $byte\"; \
         [ \"$a $b $byte\" = \"033 133 $1\" ] || exit 9; }\n";

    /// The screen of the map: the header, the inspector pane across the whole width (a pane is
    /// not a dialog), and a dialog over it when there is one.
    fn map(header: &str, cursor: Option<(&str, &str)>, dialog: Option<(&str, &[&str])>) -> String {
        let mut drawn = screen(header, dialog);
        if let Some((name, state)) = cursor {
            drawn.push_str(&place(31, 1, &boxed("SELECTED ITEM", 120, &[name, state])));
        }
        drawn
    }

    /// The rest of a session that goes well, from the end of the scan: the scan ends, `Enter`
    /// opens the folder under the cursor, `Esc` goes back, `q` asks to quit and `y` confirms.
    /// `inputs` is how many keys the program has read when the scan ends.
    fn from_the_end_of_the_scan(
        finished: &str,
        cursor: (&str, &str),
        inputs: u32,
        prompt_delay: &str,
        exit_delay: &str,
    ) -> String {
        let root = map(finished, Some(cursor), None);
        let prompt = map(finished, Some(cursor), Some(QUIT));
        let inside = map(INSIDE, Some(("a.jpg", "◆ COMPLETE · file")), None);
        format!(
            "{SCAN_COMPLETE}{root}frame {inputs}\n\
             press 015\n{inside}frame {enter}\n\
             press 033\n{root}frame {up}\n\
             press 161\n/bin/sleep {prompt_delay}\n{QUIT_PROMPT}{prompt}frame {quit}\n\
             press 171\n/bin/sleep {exit_delay}\nexit 0\n",
            enter = inputs + 1,
            up = inputs + 2,
            quit = inputs + 3,
        )
    }

    /// A session that goes well: the first `Esc` is answered, the scan ends with the header
    /// `finished` and the cursor on `cursor`, and the rest is [`from_the_end_of_the_scan`].
    fn a_session_that_goes_well(
        finished: &str,
        cursor: (&str, &str),
        prompt_delay: &str,
        exit_delay: &str,
    ) -> String {
        format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{rest}",
            scanning = map(SCANNING, None, None),
            rest = from_the_end_of_the_scan(finished, cursor, 1, prompt_delay, exit_delay),
        )
    }

    /// The bounds of these tests: roomy, so that a test narrows the one it is about.
    fn roomy() -> Limits {
        Limits {
            first_frame: Duration::from_secs(10),
            scan: Duration::from_secs(20),
            key: Duration::from_secs(5),
            drill: Duration::from_secs(10),
            quit: Duration::from_secs(10),
            ..Limits::for_run(Duration::from_secs(120))
        }
    }

    /// A tree, a work directory, and a program that is a script, in a directory of their own.
    struct Setup {
        dir: tempfile::TempDir,
        tree: PathBuf,
        work: PathBuf,
        binary: PathBuf,
    }

    impl Setup {
        fn new(script: &str) -> Self {
            let dir = tempfile::Builder::new()
                .prefix("xt-play-")
                .tempdir()
                .expect("a directory");
            let (tree, work, bin) = (
                dir.path().join("tree"),
                dir.path().join("work"),
                dir.path().join("bin"),
            );
            for path in [&tree, &work, &bin] {
                fs::create_dir(path).expect("a directory");
            }
            let binary = bin.join("excise");
            let text = format!("#!/bin/sh\n{}{script}", prelude(true))
                .replace("@DIR@", &dir.path().to_string_lossy());
            fs::write(&binary, text).expect("a script");
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("executable");
            Self {
                dir,
                tree,
                work,
                binary,
            }
        }
    }

    /// A session that was run against a script, and what the script wrote down.
    struct Played {
        run: TuiRun,
        dir: tempfile::TempDir,
        took: Duration,
    }

    impl Played {
        /// The keys the program read, one per line: the octal codes of its bytes.
        fn received(&self) -> Vec<String> {
            fs::read_to_string(self.dir.path().join("received"))
                .unwrap_or_default()
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// How many quirks of `kind` the session noted.
        fn quirks_of(&self, kind: QuirkKind) -> usize {
            self.run
                .quirks
                .quirks()
                .iter()
                .filter(|quirk| quirk.kind == kind)
                .count()
        }
    }

    /// Runs one session against `script`, with `limits` and a whole run that ends after `run`.
    fn play(script: &str, limits: &Limits, run: Duration) -> Played {
        let setup = Setup::new(script);
        let root = SoakRoot::open(&setup.tree).expect("a root");
        let interrupt = Interrupt::new();
        let context = Context {
            root: &root,
            binary: &setup.binary,
            work_dir: &setup.work,
            limits,
            run_deadline: Instant::now() + run,
            interrupt: &interrupt,
            record: false,
        };
        let started = Instant::now();
        let run = run_session(&context, 1, Profile::Default).expect("a session");
        let took = started.elapsed();
        Played {
            run,
            dir: setup.dir,
            took,
        }
    }

    /// Every key the program read is one the soak may send: an arrow, a Vim letter, Enter, Esc,
    /// `q`, or `y`. Never Backspace (`177`), Ctrl+H (`010`), or the barrier byte (`035`).
    fn assert_only_allowed_keys(received: &[String]) {
        let allowed = [
            "033",
            "015",
            "161",
            "171",
            "033 133 101",
            "033 133 102",
            "033 133 103",
            "033 133 104",
            "150",
            "152",
            "153",
            "154",
        ];
        for key in received {
            assert!(
                allowed.contains(&key.as_str()),
                "the program read `{key}`, which is no key the soak may send"
            );
        }
    }

    #[test]
    fn a_scan_that_ends_with_a_label_of_uncertainty_is_finished_and_the_session_goes_on() {
        let script = a_session_that_goes_well(
            NEEDS_REVIEW,
            ("photos", "? NEEDS REVIEW · folder"),
            "0",
            "0",
        );
        let limits = Limits {
            scan: Duration::from_secs(5),
            ..roomy()
        };

        let played = play(&script, &limits, Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.timed_out_phase, None, "{:?}", run.quirks);
        assert!(run.error.is_none() && run.end == End::Session);
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert_eq!(run.exit.code, Some(0));
        assert!(
            run.drilled,
            "the folder was opened and left: {:?}",
            run.quirks
        );
        for metric in ["complete_ms", "drill_ms", "up_ms", "quit_ms"] {
            assert!(run.metrics.contains_key(metric), "{metric}");
        }
        // The label is a quirk, counted by kind, and its text is for the local log.
        assert_eq!(
            played.quirks_of(QuirkKind::UncertainScan),
            1,
            "{:?}",
            run.quirks
        );
        let note = run
            .quirks
            .quirks()
            .iter()
            .find(|quirk| quirk.kind == QuirkKind::UncertainScan)
            .expect("the quirk");
        assert!(note.detail.contains("NEEDS REVIEW"), "{}", note.detail);
        // Every key is counted, not only the probes: the probe, `Enter`, `Esc`, `q`, and `y`.
        assert!(
            (run.metrics["inputs_sent"] - 5.0).abs() < f64::EPSILON,
            "{:?}",
            run.metrics
        );
        assert_eq!(played.received(), ["033", "015", "033", "161", "171"]);
        assert_only_allowed_keys(&played.received());

        // A scan that ends `COMPLETE` is not a quirk.
        let complete = play(
            &a_session_that_goes_well(COMPLETE, ("photos", "◆ COMPLETE · folder"), "0", "0"),
            &limits,
            Duration::from_secs(60),
        );
        assert_eq!(complete.run.exit.via, ExitVia::Quit);
        assert!(complete.run.drilled);
        assert_eq!(complete.quirks_of(QuirkKind::UncertainScan), 0);
    }

    #[test]
    fn a_name_that_reads_as_a_deletion_dialog_is_a_quirk_and_no_key_that_confirms_is_sent() {
        // The entry under the cursor is called like the first line of a deletion dialog, and it
        // is on the screen from the end of the scan to the quit. The soak asks for no deletion,
        // so no such dialog can be open: the name is what the screen holds. `Enter` and `y` stay
        // unsent, which is the refusal that keeps a real one from being confirmed, and the session
        // is no failure: the folder is not opened, and the program is killed instead of quit.
        let cursor = ("[Enter/y] start", "◆ COMPLETE · folder");
        let root = map(COMPLETE, Some(cursor), None);
        let prompt = map(COMPLETE, Some(cursor), Some(QUIT));
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n\
             {SCAN_COMPLETE}{root}frame 1\n\
             press 161\n{QUIT_PROMPT}{prompt}frame 2\n/bin/sleep 600\n",
            scanning = map(SCANNING, None, None),
        );
        let limits = Limits {
            scan: Duration::from_secs(5),
            quit: Duration::from_secs(5),
            ..roomy()
        };

        let played = play(&script, &limits, Duration::from_secs(60));

        let run = &played.run;
        assert!(
            run.error.is_none(),
            "a lookalike is no harness error: {:?}",
            run.error
        );
        assert!(
            run.end == End::Session && run.timed_out_phase.is_none(),
            "{:?}",
            run.quirks
        );
        assert!(!run.drilled, "no folder was opened");
        assert_eq!(run.exit.via, ExitVia::Killed, "{:?}", run.quirks);
        assert!(!run.quirks.has(QuirkKind::HarnessError), "{:?}", run.quirks);
        // The program read the probe and the `q`, and nothing that could confirm a dialog.
        assert_eq!(played.received(), ["033", "161"]);
        assert_only_allowed_keys(&played.received());
        let notes: Vec<&str> = run
            .quirks
            .quirks()
            .iter()
            .filter(|quirk| quirk.kind == QuirkKind::DialogText)
            .map(|quirk| quirk.detail.as_str())
            .collect();
        assert!(
            notes
                .iter()
                .any(|note| note.contains("`Enter` was not sent, so no folder was opened")),
            "{notes:?}"
        );
        assert!(
            notes
                .iter()
                .any(|note| note.contains("`y` was not sent, so the quit was not confirmed")),
            "{notes:?}"
        );
        // It is not counted as an error dialog: it is not one.
        assert_eq!(
            played.quirks_of(QuirkKind::ErrorDialog),
            0,
            "{:?}",
            run.quirks
        );
    }

    #[test]
    fn text_that_reads_as_a_deletion_dialog_on_the_screen_at_the_end_of_the_scan_is_noted_once() {
        let cursor = ("Type this exactly: ", "◆ COMPLETE · file");
        let root = map(COMPLETE, Some(cursor), None);
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n\
             {SCAN_COMPLETE}{root}frame 1\n\
             /bin/sleep 600\n",
            scanning = map(SCANNING, None, None),
        );
        let limits = Limits {
            drill: Duration::from_secs(2),
            quit: Duration::from_secs(2),
            ..roomy()
        };

        let played = play(&script, &limits, Duration::from_secs(60));

        // The first look, at the end of the scan, finds it, and a look at the same text later does
        // not make another note for it.
        assert_eq!(
            played.quirks_of(QuirkKind::DialogText),
            1,
            "{:?}",
            played.run.quirks
        );
        assert_eq!(played.quirks_of(QuirkKind::ErrorDialog), 0);
        assert_eq!(played.received(), ["033"], "no key but the probe was sent");
    }

    #[test]
    fn the_bound_on_the_scan_is_counted_from_the_first_frame_and_the_keys_cannot_postpone_it() {
        // A program that answers every key and never ends its scan. The keys are a second apart,
        // so that several are sent before the bound, and a bound that only began once they were
        // over would begin after the end of the whole run.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\nn=0\nwhile :; do press 033; n=$((n + 1)); frame $n; done\n",
            scanning = map(SCANNING, None, None),
        );
        let limits = Limits {
            scan: Duration::from_secs(2),
            ..roomy()
        };

        let played = play(&script, &limits, Duration::from_secs(10));

        let run = &played.run;
        assert_eq!(
            run.timed_out_phase,
            Some(SoakPhase::Scan),
            "{:?}",
            run.quirks
        );
        assert_eq!(
            run.end,
            End::Session,
            "the bound on the scan passed, not the run's"
        );
        assert_eq!(run.exit.via, ExitVia::Killed);
        assert!(played.took < Duration::from_secs(6), "{:?}", played.took);
        assert!(
            played.received().len() >= 2,
            "more than one key was sent before the bound: {:?}",
            played.received()
        );
    }

    #[test]
    fn the_prompt_and_the_exit_share_the_bound_on_the_quit() {
        // The prompt takes 1.3 s and so does the exit: each is inside a bound of 2 s, and
        // together they are not.
        let script =
            a_session_that_goes_well(COMPLETE, ("photos", "◆ COMPLETE · folder"), "1.3", "1.3");
        let tight = Limits {
            quit: Duration::from_secs(2),
            ..roomy()
        };

        let slow = play(&script, &tight, Duration::from_secs(60));

        assert_eq!(
            slow.run.timed_out_phase,
            Some(SoakPhase::Exit),
            "{:?}",
            slow.run.quirks
        );
        assert_eq!(slow.run.exit.via, ExitVia::Killed);

        let enough = Limits {
            quit: Duration::from_secs(5),
            ..roomy()
        };
        let fine = play(&script, &enough, Duration::from_secs(60));
        assert_eq!(fine.run.timed_out_phase, None, "{:?}", fine.run.quirks);
        assert_eq!(fine.run.exit.via, ExitVia::Quit);
    }

    #[test]
    fn a_dialog_that_comes_up_between_two_keys_is_seen_before_the_next_key_dismisses_it() {
        // The first key is answered. A while later an error dialog comes up on its own, and the
        // second key dismisses it: the frame that key draws shows no dialog, so only a look
        // before the key finds it.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n\
             /bin/sleep 0.4\n{error}frame 1\n\
             press 033\n{scanning}frame 2\n{rest}",
            scanning = map(SCANNING, None, None),
            error = map(SCANNING, None, Some(ERROR)),
            rest =
                from_the_end_of_the_scan(COMPLETE, ("photos", "◆ COMPLETE · folder"), 2, "0", "0"),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        assert_eq!(
            played.run.exit.via,
            ExitVia::Quit,
            "{:?}",
            played.run.quirks
        );
        assert_eq!(
            played.quirks_of(QuirkKind::ErrorDialog),
            1,
            "{:?}",
            played.run.quirks
        );
        let note = played
            .run
            .quirks
            .quirks()
            .iter()
            .find(|quirk| quirk.kind == QuirkKind::ErrorDialog)
            .expect("the quirk");
        assert!(
            note.detail.contains("Could not read the folder"),
            "{}",
            note.detail
        );
    }

    #[test]
    fn a_dialog_that_ignores_the_key_and_draws_no_frame_is_still_seen() {
        // The program reads the key and draws a dialog, but no frame counts the key: the wait for
        // one ends at the bound on a key, and the screen is all there is to say why.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\n{error}/bin/sleep 60\n",
            scanning = map(SCANNING, None, None),
            error = map(SCANNING, None, Some(ERROR)),
        );
        let limits = Limits {
            key: Duration::from_secs(1),
            scan: Duration::from_secs(3),
            ..roomy()
        };

        let played = play(&script, &limits, Duration::from_secs(30));

        assert_eq!(
            played.quirks_of(QuirkKind::NoFrameForKey),
            1,
            "{:?}",
            played.run.quirks
        );
        assert_eq!(
            played.quirks_of(QuirkKind::ErrorDialog),
            1,
            "{:?}",
            played.run.quirks
        );
        assert_eq!(played.run.timed_out_phase, Some(SoakPhase::Complete));
    }

    #[test]
    fn a_dialog_that_stays_on_the_screen_is_noted_once() {
        // The dialog is there from the first frame to the end of the scan, through every look.
        let script = format!(
            "{SCRIPT_HEAD}{error}frame 0\npress 033\nframe 1\npress 033\nframe 2\n{rest}",
            error = map(SCANNING, None, Some(ERROR)),
            rest =
                from_the_end_of_the_scan(COMPLETE, ("photos", "◆ COMPLETE · folder"), 2, "0", "0"),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        assert_eq!(
            played.run.exit.via,
            ExitVia::Quit,
            "{:?}",
            played.run.quirks
        );
        assert_eq!(
            played.quirks_of(QuirkKind::ErrorDialog),
            1,
            "{:?}",
            played.run.quirks
        );
    }

    #[test]
    fn a_recording_that_cannot_be_flushed_when_the_session_ends_is_a_harness_error() {
        // The program hangs, so the session ends by its bound and the program has to be killed.
        // The recording is a named pipe whose reader goes away before the end: the flush fails.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\n/bin/sleep 60\n",
            scanning = map(SCANNING, None, None)
        );
        let setup = Setup::new(&script);
        let cast = setup.dir.path().join("cast");
        nix::unistd::mkfifo(
            &cast,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("a named pipe");
        // Read and write: opening it does not wait for a writer, and the recording's own open of
        // the pipe, for writing, does not wait for a reader.
        let reader = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cast)
            .expect("a reader of the recording");
        let root = SoakRoot::open(&setup.tree).expect("a root");
        let interrupt = Interrupt::new();
        let limits = Limits {
            key: Duration::from_secs(1),
            scan: Duration::from_secs(2),
            ..roomy()
        };
        let context = Context {
            root: &root,
            binary: &setup.binary,
            work_dir: &setup.work,
            limits: &limits,
            run_deadline: Instant::now() + Duration::from_secs(30),
            interrupt: &interrupt,
            record: false,
        };
        let scratch = Scratch::create(&setup.work).expect("a scratch area");
        let session = PtySession::spawn(&spawn_spec(
            &context,
            &scratch,
            Profile::Default,
            Some(cast.as_path()),
        ))
        .expect("the program starts");
        let mut driver = Driver::new(
            Live::new(session, scratch.events()),
            scratch.store(),
            &context,
            "test".to_owned(),
        );
        let result = driver.play();
        drop(reader);

        let run = driver.conclude(result, &scratch, None, 1, Profile::Default);

        assert!(run.error.is_some(), "{:?}", run.quirks);
        assert!(run.quirks.has(QuirkKind::HarnessError));
        assert_eq!(
            run.exit.via,
            ExitVia::Killed,
            "the program was ended all the same"
        );
        assert_eq!(
            run.timed_out_phase,
            Some(SoakPhase::Complete),
            "what ended the session is not lost"
        );
    }

    #[test]
    fn a_cursor_on_a_file_is_walked_to_a_folder_with_arrow_keys_and_nothing_else() {
        // The largest entry is a file, the next is another file, and the third is a folder: the
        // walk goes right twice, and the folder is opened and left.
        let at_root = |cursor: (&str, &str)| map(COMPLETE, Some(cursor), None);
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{big}frame 1\n\
             arrow 103\n{notes}frame 2\n\
             arrow 103\n{photos}frame 3\n\
             press 015\n{inside}frame 4\n\
             press 033\n{photos}frame 5\n\
             press 161\n{QUIT_PROMPT}{quit}frame 6\n\
             press 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
            big = at_root(("big.img", "◆ COMPLETE · file")),
            notes = at_root(("notes.txt", "◆ COMPLETE · file")),
            photos = at_root(("photos", "◆ COMPLETE · folder")),
            inside = map(INSIDE, Some(("a.jpg", "◆ COMPLETE · file")), None),
            quit = map(
                COMPLETE,
                Some(("photos", "◆ COMPLETE · folder")),
                Some(QUIT)
            ),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert!(run.drilled, "{:?}", run.quirks);
        assert_eq!(played.quirks_of(QuirkKind::DrillSkipped), 0);
        assert_eq!(
            played.received(),
            [
                "033",
                "033 133 103",
                "033 133 103",
                "015",
                "033",
                "161",
                "171"
            ]
        );
        assert_only_allowed_keys(&played.received());
    }

    #[test]
    fn a_root_of_files_alone_is_walked_all_around_and_left_without_a_drill_and_without_a_quirk() {
        // Right reaches the second file; right again and down go nowhere; left comes back to the
        // first, where down, left, and up go nowhere; the keys the second still has are reached by
        // going right again: its up key goes nowhere. Every key has been tried from every entry
        // and none led to a folder: the map holds none.
        let at_root = |cursor: (&str, &str)| map(COMPLETE, Some(cursor), None);
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{a}frame 1\n\
             arrow 103\n{b}frame 2\n\
             arrow 103\nframe 3\n\
             arrow 102\nframe 4\n\
             arrow 104\n{a}frame 5\n\
             arrow 102\nframe 6\n\
             arrow 104\nframe 7\n\
             arrow 101\nframe 8\n\
             arrow 103\n{b}frame 9\n\
             arrow 101\nframe 10\n\
             press 161\n{QUIT_PROMPT}{quit}frame 11\n\
             press 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
            a = at_root(("a.txt", "◆ COMPLETE · file")),
            b = at_root(("b.txt", "◆ COMPLETE · file")),
            quit = map(COMPLETE, Some(("b.txt", "◆ COMPLETE · file")), Some(QUIT)),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert!(!run.drilled);
        assert_eq!(
            played.quirks_of(QuirkKind::DrillSkipped),
            0,
            "a map that was all walked and has no folder is no quirk: {:?}",
            run.quirks
        );
        assert_eq!(
            played.received(),
            [
                "033",
                "033 133 103",
                "033 133 103",
                "033 133 102",
                "033 133 104",
                "033 133 102",
                "033 133 104",
                "033 133 101",
                "033 133 103",
                "033 133 101",
                "161",
                "171"
            ]
        );
        assert_only_allowed_keys(&played.received());
    }

    #[test]
    fn a_walk_that_runs_out_of_keys_is_a_quirk_and_the_session_still_quits() {
        // Every key leads to an entry the walk has not seen, for ever.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{first}frame 1\n\
             i=0\nwhile :; do\n  key\n  case \"$byte\" in\n\
             033) key; key; i=$((i + 1)); log \"033 133 $byte\"; \
             printf '\\033[32;2H%-118s' \"file-$i\"; frame $((i + 1)) ;;\n\
             161) break ;;\n  *) exit 9 ;;\n  esac\ndone\n\
             log 161\n{QUIT_PROMPT}{quit}frame $((i + 2))\npress 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
            first = map(COMPLETE, Some(("file-0", "◆ COMPLETE · file")), None),
            quit = map(COMPLETE, Some(("file-0", "◆ COMPLETE · file")), Some(QUIT)),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert!(!run.drilled);
        assert_eq!(
            played.quirks_of(QuirkKind::DrillSkipped),
            1,
            "{:?}",
            run.quirks
        );
        let arrows = played
            .received()
            .iter()
            .filter(|key| key.starts_with("033 133"))
            .count();
        assert_eq!(arrows, MAX_WALK as usize, "the walk is bounded");
        assert_only_allowed_keys(&played.received());
    }

    #[test]
    fn a_folder_below_the_largest_file_is_found_after_a_detour_through_the_entry_to_its_right() {
        // The largest file is at the upper left, a second file to its right has nothing to its
        // right or below it, and a folder is below the first. A walk that gave up after four keys
        // that led nowhere new would turn back at the first file's up key, and never try its down
        // key.
        let at_root = |cursor: (&str, &str)| map(COMPLETE, Some(cursor), None);
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{big}frame 1\n\
             arrow 103\n{notes}frame 2\n\
             arrow 103\nframe 3\n\
             arrow 102\nframe 4\n\
             arrow 104\n{big}frame 5\n\
             arrow 102\n{photos}frame 6\n\
             press 015\n{inside}frame 7\n\
             press 033\n{photos}frame 8\n\
             press 161\n{QUIT_PROMPT}{quit}frame 9\n\
             press 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
            big = at_root(("big.img", "◆ COMPLETE · file")),
            notes = at_root(("notes.txt", "◆ COMPLETE · file")),
            photos = at_root(("photos", "◆ COMPLETE · folder")),
            inside = map(INSIDE, Some(("a.jpg", "◆ COMPLETE · file")), None),
            quit = map(
                COMPLETE,
                Some(("photos", "◆ COMPLETE · folder")),
                Some(QUIT)
            ),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert!(run.drilled, "{:?}", run.quirks);
        assert_eq!(played.quirks_of(QuirkKind::DrillSkipped), 0);
        assert_eq!(
            played.received(),
            [
                "033",
                "033 133 103",
                "033 133 103",
                "033 133 102",
                "033 133 104",
                "033 133 102",
                "015",
                "033",
                "161",
                "171"
            ]
        );
        assert_only_allowed_keys(&played.received());
    }

    #[test]
    fn the_walk_to_a_folder_and_its_opening_share_the_bound_on_the_drill() {
        // Every arrow key is answered, a second late, with an entry that has not been seen: a
        // walk that had a bound of its own for each key would go on for as long as it has keys.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{first}frame 1\n\
             i=0\nwhile :; do\n  key\n  case \"$byte\" in\n\
             033) key; key; i=$((i + 1)); log \"033 133 $byte\"; /bin/sleep 1; \
             printf '\\033[32;2H%-118s' \"file-$i\"; frame $((i + 1)) ;;\n\
             *) exit 9 ;;\n  esac\ndone\n",
            scanning = map(SCANNING, None, None),
            first = map(COMPLETE, Some(("file-0", "◆ COMPLETE · file")), None),
        );
        let limits = Limits {
            drill: Duration::from_secs(3),
            ..roomy()
        };

        let played = play(&script, &limits, Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(
            run.timed_out_phase,
            Some(SoakPhase::Drill),
            "{:?}",
            run.quirks
        );
        assert_eq!(run.end, End::Session);
        assert_eq!(run.exit.via, ExitVia::Killed);
        assert!(played.took < Duration::from_secs(12), "{:?}", played.took);
        let arrows = played
            .received()
            .iter()
            .filter(|key| key.starts_with("033 133"))
            .count();
        assert!(arrows < 8, "{arrows} arrow keys in a bound of 3 s");
    }

    /// A header whose path the program has cut in the middle.
    const CUT_COMPLETE: &str = " EXCISE  /scripted/very/long/pa[...]ng/path/  ◆ COMPLETE";

    #[test]
    fn a_folder_is_told_from_the_root_by_its_entry_when_the_header_cuts_both_paths_to_one_text() {
        // The root and the folder below it are two paths, and the program cuts both to the same
        // text. The header says nothing then; the inspector shows the folder under the cursor
        // before `Enter`, something in it after, and the folder again after `Esc`.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{root}frame 1\n\
             press 015\n{inside}frame 2\n\
             press 033\n{root}frame 3\n\
             press 161\n{QUIT_PROMPT}{prompt}frame 4\npress 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
            root = map(CUT_COMPLETE, Some(("photos", "◆ COMPLETE · folder")), None),
            inside = map(CUT_COMPLETE, Some(("a.jpg", "◆ COMPLETE · file")), None),
            prompt = map(
                CUT_COMPLETE,
                Some(("photos", "◆ COMPLETE · folder")),
                Some(QUIT)
            ),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert_eq!(run.timed_out_phase, None, "{:?}", run.quirks);
        assert!(run.drilled, "{:?}", run.quirks);
        assert!(
            run.metrics.contains_key("drill_ms") && run.metrics.contains_key("up_ms"),
            "{:?}",
            run.metrics
        );
        assert_eq!(played.quirks_of(QuirkKind::DrillSkipped), 0);
        assert_eq!(played.received(), ["033", "015", "033", "161", "171"]);
    }

    #[test]
    fn a_cut_path_on_a_screen_that_is_not_exact_is_not_drilled_and_no_key_is_sent() {
        // The screen of a terminal that paints on its own timer is not read for the inspector,
        // and the header's text cannot tell the folder from the root.
        with_program(
            |received| {
                format!(
                    "stty raw -echo; printf '\\033[2J\\033[1;1H%s\\033[3;1H' '{CUT_COMPLETE}'; \
                     echo ready >&2; exec /bin/cat >> '{}'",
                    received.display()
                )
            },
            |driver, received| {
                ready(driver);
                driver.live.frame_window = Duration::from_millis(50);

                let drilled = driver.drill();

                assert!(drilled.is_ok(), "{drilled:?}");
                assert!(!driver.drilled);
                assert!(driver.quirks.has(QuirkKind::DrillSkipped));
                assert_eq!(driver.live.inputs_sent(), 0, "a key was sent");
                driver
                    .wait_quietly(Duration::from_millis(150))
                    .expect("the program is driven");
                assert!(fs::read(received).unwrap_or_default().is_empty());
            },
        );
    }

    #[test]
    fn a_path_is_cut_when_it_holds_what_the_program_puts_where_it_cuts_one() {
        assert!(header_is_cut("/Users/me/Library/Applic[...]Support/Google"));
        assert!(header_is_cut("/Users/me/Library/Appli[..]pport/Google"));
        assert!(!header_is_cut("/Users/me/Library/Application Support"));
        assert!(!header_is_cut("/Users/[me]/Library"));
    }

    #[test]
    fn the_screen_is_caught_up_to_the_first_frame_before_it_is_looked_at_or_a_key_is_sent() {
        // The event of the first frame comes at once. The bytes of the frame, an error dialog and
        // then the mark, come half a second later. The first probe dismisses the dialog, so a
        // look made before the screen holds the frame would never see it.
        let script = format!(
            "{SCRIPT_HEAD}report 0\n/bin/sleep 0.5\n{error}mark\n\
             press 033\n{scanning}frame 1\n{rest}",
            scanning = map(SCANNING, None, None),
            error = map(SCANNING, None, Some(ERROR)),
            rest =
                from_the_end_of_the_scan(COMPLETE, ("photos", "◆ COMPLETE · folder"), 1, "0", "0"),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        assert_eq!(
            played.run.exit.via,
            ExitVia::Quit,
            "{:?}",
            played.run.quirks
        );
        assert_eq!(
            played.quirks_of(QuirkKind::ErrorDialog),
            1,
            "{:?}",
            played.run.quirks
        );
    }

    #[test]
    fn the_end_of_the_scan_is_read_from_a_frame_the_screen_shows_and_not_from_half_a_redraw() {
        // The program says that the scan is over and starts to redraw the badge: for 0.6 s the
        // screen holds a label that is half of `COMPLETE`, and then the whole frame, with the
        // mark that says so. That label is the label of no scan, and the time the scan ended
        // on the screen is the time of the frame.
        let half = place(1, 1, &[" EXCISE  /scripted/  ◆ COMPNING".to_owned()]);
        let root = map(COMPLETE, Some(("photos", "◆ COMPLETE · folder")), None);
        let inside = map(INSIDE, Some(("a.jpg", "◆ COMPLETE · file")), None);
        let prompt = map(
            COMPLETE,
            Some(("photos", "◆ COMPLETE · folder")),
            Some(QUIT),
        );
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n\
             {SCAN_COMPLETE}{half}/bin/sleep 0.6\n{root}frame 1\n\
             press 015\n{inside}frame 2\n\
             press 033\n{root}frame 3\n\
             press 161\n{QUIT_PROMPT}{prompt}frame 4\n\
             press 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert_eq!(
            played.quirks_of(QuirkKind::UncertainScan),
            0,
            "half of a badge is no label: {:?}",
            run.quirks
        );
        assert!(
            run.metrics["complete_ms"] >= 600.0,
            "the scan ended on the screen with the frame: {:?}",
            run.metrics
        );
    }

    #[test]
    fn a_frame_whose_mark_never_comes_ends_the_wait_for_the_scan_at_its_bound() {
        // The scan is over and the screen has the root, but the program never writes the mark of
        // the frame that says so. The wait for the screen to show the frame is inside the bound on
        // the scan: it does not get a bound of its own on top of it, and what follows is not
        // done on a screen that was never tied to a frame.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n\
             {SCAN_COMPLETE}{root}report 1\n/bin/sleep 60\n",
            scanning = map(SCANNING, None, None),
            root = map(COMPLETE, Some(("photos", "◆ COMPLETE · folder")), None),
        );
        let limits = Limits {
            scan: Duration::from_secs(2),
            ..roomy()
        };

        let played = play(&script, &limits, Duration::from_secs(30));

        let run = &played.run;
        assert_eq!(
            run.timed_out_phase,
            Some(SoakPhase::Complete),
            "{:?}",
            run.quirks
        );
        assert_eq!(run.end, End::Session);
        assert!(!run.drilled);
        assert!(played.took < Duration::from_secs(8), "{:?}", played.took);
    }

    #[test]
    fn the_times_of_the_drill_and_of_the_way_back_end_with_the_frame_that_shows_them() {
        // The header of the folder, with a badge that says its scan ended, is on the screen a
        // while before the rest of the frame and its mark: so is the header of the root when the
        // way back is made. Neither is the end of its time.
        let root = map(COMPLETE, Some(("photos", "◆ COMPLETE · folder")), None);
        let inside = map(INSIDE, Some(("a.jpg", "◆ COMPLETE · file")), None);
        let prompt = map(
            COMPLETE,
            Some(("photos", "◆ COMPLETE · folder")),
            Some(QUIT),
        );
        let inside_header = place(1, 1, &[INSIDE.to_owned()]);
        let root_header = place(1, 1, &[COMPLETE.to_owned()]);
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{root}frame 1\n\
             press 015\n{inside_header}/bin/sleep 0.6\n{inside}frame 2\n\
             press 033\n{root_header}/bin/sleep 0.6\n{root}frame 3\n\
             press 161\n{QUIT_PROMPT}{prompt}frame 4\n\
             press 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert!(run.drilled, "{:?}", run.quirks);
        assert!(
            run.metrics["drill_ms"] >= 600.0,
            "the folder was shown with the frame: {:?}",
            run.metrics
        );
        assert!(
            run.metrics["up_ms"] >= 600.0,
            "the root was shown with the frame: {:?}",
            run.metrics
        );
    }

    #[test]
    fn a_key_whose_recording_fails_after_it_was_written_is_still_counted() {
        // The program reads keys for ever. The recording is a named pipe whose reader goes away,
        // and what is written to it is buffered: the key that fills the buffer is written to the
        // program first and then fails in the recording, which is made after the write.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\nwhile :; do key; log \"$byte\"; done\n",
            scanning = map(SCANNING, None, None),
        );
        let setup = Setup::new(&script);
        let cast = setup.dir.path().join("cast");
        nix::unistd::mkfifo(
            &cast,
            nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
        )
        .expect("a named pipe");
        // Read and write: opening it does not wait for a writer, and the recording's own open of
        // the pipe, for writing, does not wait for a reader.
        let reader = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cast)
            .expect("a reader of the recording");
        let root = SoakRoot::open(&setup.tree).expect("a root");
        let interrupt = Interrupt::new();
        let limits = roomy();
        let context = Context {
            root: &root,
            binary: &setup.binary,
            work_dir: &setup.work,
            limits: &limits,
            run_deadline: Instant::now() + Duration::from_secs(60),
            interrupt: &interrupt,
            record: false,
        };
        let scratch = Scratch::create(&setup.work).expect("a scratch area");
        let session = PtySession::spawn(&spawn_spec(
            &context,
            &scratch,
            Profile::Default,
            Some(cast.as_path()),
        ))
        .expect("the program starts");
        let mut driver = Driver::new(
            Live::new(session, scratch.events()),
            scratch.store(),
            &context,
            "test".to_owned(),
        );
        driver.first_frame().expect("the first frame");
        drop(reader);

        let mut attempts = 0_u32;
        let error = loop {
            assert!(attempts < 5_000, "no key ever failed to be recorded");
            attempts += 1;
            if let Err(error) = driver.press(Key::J) {
                break error;
            }
        };

        assert!(
            matches!(error, RunError::Pty(crate::pty::PtyError::Recording { .. })),
            "a failure after the write: {error:?}"
        );
        assert_eq!(driver.live.inputs_sent(), u64::from(attempts));
        let metrics = driver.metrics();
        assert!(
            (metrics["inputs_sent"] - f64::from(attempts)).abs() < f64::EPSILON,
            "the key whose recording failed is counted: {metrics:?}"
        );
    }

    #[test]
    fn a_scratch_area_that_cannot_be_read_is_a_harness_error_and_not_residue() {
        use std::os::unix::fs::PermissionsExt as _;

        // A process that is root can list every folder, and there is nothing to try.
        let probe = tempfile::Builder::new()
            .prefix("xt-probe-")
            .tempdir()
            .expect("a directory");
        let locked = probe.path().join("locked");
        fs::create_dir(&locked).expect("a directory");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
        let listable = fs::read_dir(&locked).is_ok();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod");
        if listable {
            eprintln!("skipped: a process that is root can list every folder");
            return;
        }

        let setup = Setup::new("exit 0\n");
        let root = SoakRoot::open(&setup.tree).expect("a root");
        let interrupt = Interrupt::new();
        let limits = roomy();
        let context = Context {
            root: &root,
            binary: &setup.binary,
            work_dir: &setup.work,
            limits: &limits,
            run_deadline: Instant::now() + Duration::from_secs(60),
            interrupt: &interrupt,
            record: false,
        };
        let scratch = Scratch::create(&setup.work).expect("a scratch area");
        let session = PtySession::spawn(&spawn_spec(&context, &scratch, Profile::Default, None))
            .expect("the program starts");
        let driver = Driver::new(
            Live::new(session, scratch.events()),
            scratch.store(),
            &context,
            "test".to_owned(),
        );
        fs::set_permissions(scratch.home(), fs::Permissions::from_mode(0o000)).expect("chmod");

        let run = driver.conclude(Ok(()), &scratch, None, 1, Profile::Default);
        fs::set_permissions(scratch.home(), fs::Permissions::from_mode(0o700)).expect("chmod");

        assert!(
            run.residue.is_empty(),
            "no entry is made up for an area that could not be read: {:?}",
            run.residue
        );
        assert!(run.error.is_some(), "{:?}", run.quirks);
        assert!(run.quirks.has(QuirkKind::HarnessError));
        assert!(!run.quirks.has(QuirkKind::Residue));
    }

    // -----------------------------------------------------------------------------------------
    // What a read of the terminal can end in the middle of, the entries that look alike, and the
    // end of the program.

    /// [`map`] with a pane of the lines given, which is what `excise` shows of an entry besides its
    /// name and its state.
    fn map_with_pane(header: &str, lines: &[&str], dialog: Option<(&str, &[&str])>) -> String {
        let mut drawn = screen(header, dialog);
        drawn.push_str(&place(31, 1, &boxed("SELECTED ITEM", 120, lines)));
        drawn
    }

    #[test]
    fn a_read_that_brings_the_first_output_of_the_next_frame_does_not_change_the_frame_that_is_read()
     {
        // The mark of the frame that ends the scan and the start of the next redraw, which cuts
        // the screen and writes half of the badge, come in one write, so in one read of the
        // terminal. The screen model then holds half a redraw, and the frame that was marked is
        // the one the soak reads: its badge says `COMPLETE`, and the folder under its cursor is
        // opened. The half of the badge is no label of the scan.
        let root = map(COMPLETE, Some(("photos", "◆ COMPLETE · folder")), None);
        let inside = map(INSIDE, Some(("a.jpg", "◆ COMPLETE · file")), None);
        let prompt = map(
            COMPLETE,
            Some(("photos", "◆ COMPLETE · folder")),
            Some(QUIT),
        );
        let half_of_the_next_frame = "printf '\\033]9471;excise-frame=%s\\007\\033[2J\\033[1;1H%s' \
                                      \"$seq\" ' EXCISE  /scripted/  ◆ COMPNING'\n";
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n\
             {SCAN_COMPLETE}{root}report 1\n{half_of_the_next_frame}\
             press 015\n{inside}frame 2\n\
             press 033\n{root}frame 3\n\
             press 161\n{QUIT_PROMPT}{prompt}frame 4\n\
             press 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert_eq!(
            played.quirks_of(QuirkKind::UncertainScan),
            0,
            "half of a badge is no label: {:?}",
            run.quirks
        );
        assert!(
            run.drilled,
            "the folder under the cursor of the marked frame was opened: {:?}",
            run.quirks
        );
        assert_eq!(played.received(), ["033", "015", "033", "161", "171"]);
    }

    #[test]
    fn two_entries_whose_names_are_cut_to_one_text_are_two_entries_to_the_walk() {
        // The largest file and the file to its right show one name, which the pane has cut, and
        // their item checks differ. A walk that kept the entries by name would take the move to
        // the second for a key that went nowhere, press nothing from it, and give up on a map
        // that has a folder to the right of both.
        let cut = "a-name-that-the-program-cuts-in-the-mi[...]ll-so-that-two-read-alike.dat";
        let at_root = |lines: &[&str]| map_with_pane(COMPLETE, lines, None);
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{first}frame 1\n\
             arrow 103\n{second}frame 2\n\
             arrow 103\n{photos}frame 3\n\
             press 015\n{inside}frame 4\n\
             press 033\n{photos}frame 5\n\
             press 161\n{QUIT_PROMPT}{quit}frame 6\n\
             press 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
            first = at_root(&[cut, "◆ COMPLETE · file", "Item check: 1"]),
            second = at_root(&[cut, "◆ COMPLETE · file", "Item check: 2"]),
            photos = at_root(&["photos", "◆ COMPLETE · folder", "Item check: 3"]),
            inside = map(INSIDE, Some(("a.jpg", "◆ COMPLETE · file")), None),
            quit = map_with_pane(
                COMPLETE,
                &["photos", "◆ COMPLETE · folder", "Item check: 3"],
                Some(QUIT)
            ),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert!(run.drilled, "{:?}", run.quirks);
        assert_eq!(played.quirks_of(QuirkKind::DrillSkipped), 0);
        assert_eq!(
            played.received(),
            [
                "033",
                "033 133 103",
                "033 133 103",
                "015",
                "033",
                "161",
                "171"
            ]
        );
        assert_only_allowed_keys(&played.received());
    }

    #[test]
    fn a_folder_with_nothing_in_it_is_shown_even_when_the_header_does_not_change() {
        // The program cuts the paths of the root and of the empty folder below it to one text, and
        // the inspector of a folder with nothing in it shows no entry: neither the header nor an
        // entry says that the folder is open, and the pane that invites a choice does.
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\npress 033\nframe 1\n{SCAN_COMPLETE}{root}frame 1\n\
             press 015\n{inside}frame 2\n\
             press 033\n{root}frame 3\n\
             press 161\n{QUIT_PROMPT}{prompt}frame 4\npress 171\nexit 0\n",
            scanning = map(SCANNING, None, None),
            root = map(CUT_COMPLETE, Some(("photos", "◆ COMPLETE · folder")), None),
            inside = map_with_pane(
                CUT_COMPLETE,
                &["Choose an item to see its space, deletion options, and scan status."],
                None
            ),
            prompt = map(
                CUT_COMPLETE,
                Some(("photos", "◆ COMPLETE · folder")),
                Some(QUIT)
            ),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.timed_out_phase, None, "{:?}", run.quirks);
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert!(run.drilled, "{:?}", run.quirks);
        assert!(
            run.metrics.contains_key("drill_ms") && run.metrics.contains_key("up_ms"),
            "{:?}",
            run.metrics
        );
        assert_eq!(played.received(), ["033", "015", "033", "161", "171"]);
    }

    /// A driver over a session against `script`, with `interrupt` as the person's interrupt, and
    /// the scratch area the session runs in, which the end of the session reads. The program is
    /// ended when the body is done.
    fn with_driver<R>(
        script: &str,
        interrupt: &Interrupt,
        body: impl FnOnce(Driver<'_>, &Scratch) -> R,
    ) -> R {
        let setup = Setup::new(script);
        let root = SoakRoot::open(&setup.tree).expect("a root");
        let limits = roomy();
        let context = Context {
            root: &root,
            binary: &setup.binary,
            work_dir: &setup.work,
            limits: &limits,
            run_deadline: Instant::now() + Duration::from_secs(60),
            interrupt,
            record: false,
        };
        let scratch = Scratch::create(&setup.work).expect("a scratch area");
        let session = PtySession::spawn(&spawn_spec(&context, &scratch, Profile::Default, None))
            .expect("the program starts");
        let driver = Driver::new(
            Live::new(session, scratch.events()),
            scratch.store(),
            &context,
            "test".to_owned(),
        );
        body(driver, &scratch)
    }

    /// A script that draws its first frame and then reads keys for ever.
    fn a_program_that_reads_keys() -> String {
        format!(
            "{SCRIPT_HEAD}{scanning}frame 0\nwhile :; do key; log \"$byte\"; done\n",
            scanning = map(SCANNING, None, None),
        )
    }

    #[test]
    fn an_interrupt_that_a_wait_reads_ends_the_wait_with_an_error_and_not_with_its_answer() {
        // The program has drawn its frame, so what the wait asks is answered by what has been read
        // already: the answer is there when the interrupt is seen. The kill that the interrupt
        // asks for comes first all the same, and the wait does not answer a driver that would
        // go on to send a key to a program that is gone.
        let interrupt = Interrupt::new();
        with_driver(&a_program_that_reads_keys(), &interrupt, |mut driver, _| {
            driver.first_frame().expect("the first frame");
            interrupt.trigger();

            let waited = driver.wait_until(Instant::now() + PATIENCE, |_| Some(()));

            assert!(
                matches!(&waited, Err(RunError::Io { source, .. })
                    if source.kind() == io::ErrorKind::Interrupted),
                "{waited:?}"
            );
            assert!(driver.killed_for_interrupt);
            assert!(
                driver.live.session.exit().is_some(),
                "the program's process group was killed"
            );
        });
    }

    #[test]
    fn a_session_whose_program_was_killed_for_an_interrupt_is_an_interrupted_one() {
        // The frame the session waits for has been read when the interrupt is seen, and what the
        // session would do next, a key to a program that is gone, would fail and be a harness
        // error. It is an interrupted session, with a program that was killed, and nothing odd to
        // say of the end of a program that was killed on purpose.
        let interrupt = Interrupt::new();
        with_driver(
            &a_program_that_reads_keys(),
            &interrupt,
            |mut driver, scratch| {
                driver.first_frame().expect("the first frame");
                interrupt.trigger();

                let result = driver.play();
                let run = driver.conclude(result, scratch, None, 1, Profile::Default);

                assert_eq!(run.end, End::Interrupted, "{:?}", run.quirks);
                assert!(run.error.is_none(), "{:?}", run.error);
                assert_eq!(run.exit.via, ExitVia::Killed);
                assert!(!run.quirks.has(QuirkKind::HarnessError), "{:?}", run.quirks);
                assert!(!run.quirks.has(QuirkKind::NonZeroExit), "{:?}", run.quirks);
            },
        );
    }

    #[test]
    fn the_session_is_a_program_the_soak_has_to_end_until_it_has_been_ended() {
        use std::{
            sync::atomic::{AtomicBool, Ordering},
            thread,
        };

        let setup = Setup::new(&a_program_that_reads_keys());
        let root = SoakRoot::open(&setup.tree).expect("a root");
        let interrupt = Interrupt::new();
        let limits = roomy();
        let context = Context {
            root: &root,
            binary: &setup.binary,
            work_dir: &setup.work,
            limits: &limits,
            run_deadline: Instant::now() + Duration::from_secs(60),
            interrupt: &interrupt,
            record: false,
        };
        let held_while_it_ran = AtomicBool::new(false);

        let run = thread::scope(|scope| {
            scope.spawn(|| {
                let give_up = Instant::now() + Duration::from_secs(10);
                while interrupt.supervised() == 0 && Instant::now() < give_up {
                    thread::sleep(Duration::from_millis(5));
                }
                held_while_it_ran.store(interrupt.supervised() == 1, Ordering::SeqCst);
                interrupt.trigger();
            });
            run_session(&context, 1, Profile::Default)
        })
        .expect("a session");

        assert!(
            held_while_it_ran.load(Ordering::SeqCst),
            "the soak held a program to end while the session ran"
        );
        assert_eq!(run.end, End::Interrupted, "{:?}", run.quirks);
        assert_eq!(interrupt.supervised(), 0, "the program has been ended");
        assert!(interrupt.is_acted_on(), "and the interrupt acted on");
    }

    #[test]
    fn the_program_is_let_go_of_before_what_it_left_behind_is_looked_at() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        // The look at the scratch area is file system work: it can block for as long as a file
        // system takes to answer, and the exit is not armed while a program is held. The stand-in
        // for the look says what the interrupt held, and whether the exit was armed, when it was
        // made.
        static HELD: AtomicUsize = AtomicUsize::new(usize::MAX);
        static ARMED: AtomicBool = AtomicBool::new(false);

        #[allow(
            clippy::unnecessary_wraps,
            reason = "it stands in for `look_at_residue`, which can fail, and has its type"
        )]
        fn looks_at_what_is_held(
            _: Instant,
            interrupt: &Interrupt,
            _: &Scratch,
        ) -> Result<ScanResidue, ScratchError> {
            HELD.store(interrupt.supervised(), Ordering::SeqCst);
            ARMED.store(interrupt.is_acted_on(), Ordering::SeqCst);
            Ok(ScanResidue::default())
        }

        let interrupt = Interrupt::new();
        with_driver(
            &a_program_that_reads_keys(),
            &interrupt,
            |mut driver, scratch| {
                driver.first_frame().expect("the first frame");
                driver.supervision = Some(interrupt.supervising().expect("a program may start"));
                driver.look_at_residue = looks_at_what_is_held;
                assert_eq!(
                    interrupt.supervised(),
                    1,
                    "the program is held while it runs"
                );
                interrupt.trigger();

                let result = driver.play();
                let run = driver.conclude(result, scratch, None, 1, Profile::Default);

                assert_eq!(run.end, End::Interrupted, "{:?}", run.quirks);
            },
        );

        assert_eq!(
            HELD.load(Ordering::SeqCst),
            0,
            "the program had been ended and let go of when the scratch area was looked at"
        );
        assert!(
            ARMED.load(Ordering::SeqCst),
            "and the exit was armed by then, so that a second press ends a command that the look blocks"
        );
    }

    #[test]
    fn a_session_is_not_started_once_the_person_has_asked_to_stop() {
        let setup = Setup::new(&a_program_that_reads_keys());
        let root = SoakRoot::open(&setup.tree).expect("a root");
        let interrupt = Interrupt::new();
        interrupt.trigger();
        let limits = roomy();
        let context = Context {
            root: &root,
            binary: &setup.binary,
            work_dir: &setup.work,
            limits: &limits,
            run_deadline: Instant::now() + Duration::from_secs(60),
            interrupt: &interrupt,
            record: false,
        };

        let run = run_session(&context, 1, Profile::Default);

        assert!(
            matches!(run, Err(SoakError::NotStarted(_))),
            "the interrupt came before the program was started: {run:?}"
        );
        assert_eq!(interrupt.supervised(), 0);
        let left: Vec<_> = fs::read_dir(&setup.work)
            .expect("the work directory")
            .flatten()
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            left.is_empty(),
            "the scratch area of a session that was not started was removed: {left:?}"
        );
    }

    #[test]
    fn a_program_that_ended_by_itself_before_the_interrupt_is_not_made_an_interrupted_one() {
        // The program has quit, and the person's interrupt comes after: nothing was cut short.
        let interrupt = Interrupt::new();
        let script = format!(
            "{SCRIPT_HEAD}{scanning}frame 0\nexit 0\n",
            scanning = map(SCANNING, None, None)
        );
        with_driver(&script, &interrupt, |mut driver, scratch| {
            driver.first_frame().expect("the first frame");
            let waited = driver
                .wait_until(Instant::now() + PATIENCE, |driver| {
                    driver.live.session.exit().map(|_| ())
                })
                .expect("the program is driven");
            assert!(matches!(waited, Waited::Ready(())));
            interrupt.trigger();

            let seen = driver.pump();
            let run = driver.conclude(Ok(()), scratch, None, 1, Profile::Default);

            assert!(seen.is_ok(), "{seen:?}");
            assert!(!driver_was_killed(&run));
            assert_eq!(run.end, End::Session);
            assert_eq!(run.exit.code, Some(0));
        });
    }

    /// Whether a session ended with the program killed.
    fn driver_was_killed(run: &TuiRun) -> bool {
        run.exit.via == ExitVia::Killed
    }

    #[test]
    fn a_program_that_is_still_there_after_the_kill_is_a_harness_error_and_not_an_ended_session() {
        use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};

        // Somebody else reaps the program, so that the session can never see it end: the kill of
        // its process group goes to nothing, and the program is "still there" for as long as the
        // time the kill is given. That is what a program stuck where a signal does not reach it
        // looks like from here.
        let interrupt = Interrupt::new();
        with_driver(
            &a_program_that_reads_keys(),
            &interrupt,
            |mut driver, scratch| {
                driver.first_frame().expect("the first frame");
                let pid = driver.live.session.pid();
                let _ = crate::safety::kill_process_group(pid);
                let leader = Pid::from_raw(i32::try_from(pid).expect("a pid")).expect("a pid");
                waitid(WaitId::Pid(leader), WaitIdOptions::EXITED).expect("the program is reaped");

                let run = driver.conclude(Ok(()), scratch, None, 1, Profile::Default);

                let error = run.error.as_ref().expect("a harness error");
                assert!(error.to_string().contains("could not be ended"), "{error}");
                assert!(run.quirks.has(QuirkKind::HarnessError), "{:?}", run.quirks);
                assert_eq!(run.exit.via, ExitVia::Killed);
            },
        );
    }

    #[test]
    fn what_a_program_leaves_in_its_scratch_area_is_counted_and_only_the_first_paths_are_named() {
        let script = format!(
            "{SCRIPT_HEAD}for i in $(seq 1 25); do : > \"left-$i\"; done\n\
             {scanning}frame 0\npress 033\nframe 1\n{rest}",
            scanning = map(SCANNING, None, None),
            rest =
                from_the_end_of_the_scan(COMPLETE, ("photos", "◆ COMPLETE · folder"), 1, "0", "0"),
        );

        let played = play(&script, &roomy(), Duration::from_secs(60));

        let run = &played.run;
        assert_eq!(run.exit.via, ExitVia::Quit, "{:?}", run.quirks);
        assert_eq!(run.residue.found, 25, "{:?}", run.residue);
        assert_eq!(run.residue.names.len(), 20, "{:?}", run.residue);
        assert_eq!(run.residue.cut, None);
        assert_eq!(played.quirks_of(QuirkKind::Residue), 1, "{:?}", run.quirks);
        let note = run
            .quirks
            .quirks()
            .iter()
            .find(|quirk| quirk.kind == QuirkKind::Residue)
            .expect("the quirk");
        assert!(
            note.detail.starts_with("the program left 25 entries")
                && note.detail.contains("and 5 more"),
            "{}",
            note.detail
        );
    }
}
