//! The supervisor of one session: the process that owns the program, its pseudo-terminal, its
//! screen model, and its event reader, and serves the session's commands.
//!
//! The command that opens a session starts a supervisor and leaves, so the session outlives it.
//! The supervisor holds the session's lock for as long as it lives (see [`super::stale`]), makes
//! the workspace (a run copy of the fixture and a scratch area), starts `excise` on the run copy in
//! a pseudo-terminal, waits for the first frame, and announces itself by writing `ready.json`.
//! Then it serves requests until the session ends.
//!
//! # Where a session ends
//!
//! * `close`: the program is quit the way a user quits it, and killed with its process group if it
//!   does not end within a bounded wait.
//! * The program ends while a command is running (it quit, it crashed): the command's reply says
//!   how it ended.
//! * The program ends with no command running, or the session has been idle for its timeout (15
//!   minutes unless `open` said otherwise): the supervisor closes the session itself, as `close`
//!   does, and nobody is told.
//!
//! In each case the supervisor then removes the run copy, the scratch area, and the session
//! directory, after the command that is waiting for the last reply has read it. A recording, if
//! the session made one, is kept.
//!
//! # Deletions
//!
//! Every deletion goes through [`Drive::confirm_deletion`](crate::runner::live::Drive), the
//! protocol the scenario `delete` step runs: Backspace is sent, the program is asked for an input
//! barrier behind it, the dialog is read from a screen that shows the frame that answers, checked
//! with `verify` against the entry and the fixture and every sentinel, the fixture's ownership
//! marker is looked at once more, and only then is the confirmation key sent. The driver sends
//! that key nowhere else. `delete` refuses the ownership marker as a target, and refuses a program
//! that does not mark its frames or does not answer a barrier (see "The input barrier" in
//! `crate::runner::live`), before it sends a key. `keys` refuses to send a key that could confirm
//! a deletion dialog:
//!
//! * not while the screen, read after a barrier, shows a deletion dialog (or anything that reads
//!   like one). The barrier is written behind every earlier key, so once it is answered and the
//!   screen shows the frame that answers it, the program has read them all, however the terminal
//!   cut their bytes into events (`alt+backspace` can reach a program as Esc and then Backspace),
//!   and the screen shows what they did. A dialog opens on an input and never by itself, so a
//!   dialog that the screen does not show then is not about to open. A key that changes nothing
//!   draws no frame, and needs none: the barrier makes one. How much time has passed decides
//!   nothing;
//! * not at all when the program does not mark its frames or does not answer a barrier, since
//!   nothing then says that the screen shows what the program has read;
//! * not in a command that sent a key that could ask for a deletion dialog (Backspace, or bytes
//!   that finish an escape sequence) before it: the dialog that key opens is read in a command of
//!   its own, after `screen` shows what is open, and an agent decides on it;
//! * not as a single input that is the escape key and a confirmation (`alt+y`, `alt+enter`): a
//!   program may read it as two, and the escape can uncover a deletion dialog that the key then
//!   confirms, with no barrier possible between them;
//! * not as a key that continues an escape sequence that an earlier key began and nothing has
//!   finished (`alt+[`, and then `1`): the program joins the bytes of a sequence across writes, so
//!   what follows can finish it as `y` or Enter (`ESC [ 1 2 1 u`) with no `y` or Enter byte in any
//!   write, and no barrier can be written behind it, because the program takes the barrier for a
//!   part of the sequence and never answers it. A key that begins a sequence is sent, as nothing
//!   can be confirmed with it yet, and every key after it is refused: the session is closed and
//!   opened again.
//!   [`Live`] feeds one [`InputScan`](crate::pty::input::InputScan) with every byte written to the
//!   program (a command's keys, the barriers, the protocols' own writes), across commands, so
//!   the key that continues a sequence is refused whichever command it comes in.
//!
//! The sentinels of a deletion are the entries of the fixture's first two levels that are neither
//! the target, nor inside it, nor above it; the fixture as a whole is compared with its state
//! before the first deletion afterwards, so a deletion that removed anything but its target is
//! reported.
//!
//! A deletion acts on an entry of the folder the program shows, because the filter selects among
//! that folder's entries and no others. `delete` reads the folder from the header's path first,
//! and fails at once, naming both folders, when the entry is somewhere else.
//!
//! A deletion that an earlier command confirmed and gave up waiting for is still running, and the
//! program can queue another behind it. `delete` waits for such a deletion first, so that the
//! `deletion_finished` it waits for is the one its own confirmation started.

use std::{
    env,
    ffi::OsStr,
    fmt::Write as _,
    fs::{File, OpenOptions, TryLockError},
    path::{Path, PathBuf},
    process, thread,
    time::{Duration, Instant},
};

use thiserror::Error;

use super::{
    identity::{NONCE_VARIABLE, ProcessIdentity},
    keys::{Input, parse_keys},
    layout::{self, Config, SessionDir, SessionId, State, Workspace},
    mailbox::Request,
    screen_info::{
        delete_dialog_info, events_page, exit_info, modes_info, open_folder, screen_info,
    },
};
use crate::{
    events::{Event, Payload},
    fixture::{Fixtures, RunCopy, is_marker_path, spec::Part},
    pty::{
        PtyError, PtySession, SpawnSpec,
        input::is_escape_then_confirmation,
        keys::encode_text,
        ui::{
            DialogView, Inspector, deletion_dialog_visible, dialog_view, filter_prompt,
            header_state, inspector, quit_confirmation_key,
        },
    },
    report::{
        HarnessTui,
        tui::{
            Cleanup, CloseResult, DeleteResult, EventsDigest, EventsResult, ExitInfo, ExitVia,
            FixtureChanges, FrameInfo, InputCounts, KeysResult, OpenResult, ScreenInfo,
            ScreenResult, SentKey, Size, TuiCommand, TuiError, TuiErrorKind, TuiResult,
        },
    },
    runner::{
        FailureCause, RunError,
        live::{DeletionRequest, Drive, Live, ProtocolError, Waited},
        resolve_binary,
    },
    safety::{FixtureRoot, FixtureSnapshot, Scratch, isolated_env},
    scenario::{ConfirmKey, EntryKind, ScanState, check_fixture_relative_path},
};

/// How long the program has to draw its first frame.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(60);
/// How long the quit dialog has to open when a session is closed.
const QUIT_PROMPT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long the program has to end after the quit was confirmed, before its process group is
/// killed.
const QUIT_EXIT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a key that closes a dialog or a prompt waits for the frame that shows it.
const DISMISS_TIMEOUT: Duration = Duration::from_secs(1);
/// How long a command that is waiting for the session's last reply has to read it.
const REPLY_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the rest of a finished program's output is read.
const EXIT_OUTPUT_LIMIT: Duration = Duration::from_secs(2);
/// How long the output of a `keys` command has to be quiet before its screen is read, once the
/// frame that counts its keys is on the screen.
///
/// The program goes on drawing after a key (a map that opens a folder takes a moment to move), and
/// someone reads this screen, so it is worth a few more milliseconds. What the screen shows about
/// the key itself is exact without it, for a program that marks its frames.
const KEYS_QUIET: Duration = Duration::from_millis(25);
/// The most the extra read after `keys` takes, for a program that keeps drawing.
const KEYS_LIMIT: Duration = Duration::from_millis(250);
/// How long the supervisor tries for the lock, which a command asking whether it is alive can
/// hold for an instant.
const LOCK_ATTEMPTS: u32 = 40;
/// The most sentinels a deletion checks.
const MAX_SENTINELS: usize = 64;
/// The depth of the entries that are sentinels: the fixture's top level, and one level down.
const SENTINEL_DEPTH: usize = 2;
/// How long the program has to show the session's mark before what the system says about it is
/// recorded. It shows it as soon as it is loaded, which takes a moment.
const IDENTITY_WAIT: Duration = Duration::from_secs(2);
/// The environment variable that paces how fast the supervisor reads the program's terminal
/// output, in bytes per second.
///
/// It is not an option of any command, and a session that is not started by a test never sets it.
/// The driver's own tests set it to put the screen seconds behind the program, as a slow terminal
/// does (the `terminal.drain_bytes_per_sec` of a scenario), which is the state `keys` has to be
/// safe in. It is read once, when the session starts.
const DRAIN_CAP_VARIABLE: &str = "EXCISE_HARNESS_TUI_DRAIN_BYTES_PER_SEC";

/// The pace that [`DRAIN_CAP_VARIABLE`] asks for: none when it is not set.
fn drain_cap() -> Result<Option<u64>, TuiError> {
    parse_drain_cap(env::var_os(DRAIN_CAP_VARIABLE).as_deref())
}

/// What a value of [`DRAIN_CAP_VARIABLE`] asks for: nothing when there is none, and otherwise a
/// positive whole number of bytes per second. Anything else fails the start of the session, so
/// that a test that asked for a slow terminal never runs on a fast one.
fn parse_drain_cap(value: Option<&OsStr>) -> Result<Option<u64>, TuiError> {
    let Some(value) = value else {
        return Ok(None);
    };
    match value.to_str().and_then(|text| text.parse::<u64>().ok()) {
        Some(rate) if rate > 0 => Ok(Some(rate)),
        _ => Err(failed(format!(
            "`{DRAIN_CAP_VARIABLE}` is `{}`, which is not a positive whole number of bytes per \
             second",
            value.display()
        ))),
    }
}

/// The supervisor could not run.
#[derive(Debug, Error)]
pub enum ServeError {
    /// The path is not the directory of a session.
    #[error("`{}` is not the directory of a session", .0.display())]
    NotASession(PathBuf),
    /// The session's configuration cannot be read.
    #[error("cannot read the session's configuration: {0}")]
    Config(std::io::Error),
    /// Another supervisor holds the session's lock.
    #[error("another supervisor holds the lock of this session")]
    Locked,
    /// The lock cannot be taken.
    #[error("cannot lock the session: {0}")]
    Lock(std::io::Error),
    /// The session's state cannot be written.
    #[error("cannot write the session's state: {0}")]
    State(std::io::Error),
}

/// Runs the supervisor of the session whose directory is `session_dir`, until the session ends.
///
/// This is what the supervisor process runs; `cargo xtask tui` starts it with its hidden
/// `supervise` command.
///
/// # Errors
///
/// Returns why the supervisor could not run at all. A session that fails to start is reported to
/// the command that opened it, not here.
pub fn serve(session_dir: &Path) -> Result<(), ServeError> {
    let dir = session_dir_of(session_dir)?;
    let config = dir.read_config().map_err(ServeError::Config)?;
    let _lock = acquire_lock(&dir)?;
    let mut state = State {
        supervisor_pid: process::id(),
        ready: false,
        root: None,
        child_pid: None,
        child: None,
    };
    dir.write_state(&state).map_err(ServeError::State)?;

    match Session::start(&dir, &config, &mut state) {
        Ok(session) => {
            session.run();
            Ok(())
        }
        Err(error) => {
            // Whatever the start made is gone with its owners, and the workspace with them.
            let document =
                HarnessTui::failure(Some(TuiCommand::Open), Some(dir.id().to_string()), error);
            match serde_json::to_vec_pretty(&document) {
                Ok(bytes) => {
                    if let Err(error) = layout::write_atomic(&dir.failed(), &bytes) {
                        eprintln!("cannot write the failure: {error}");
                    }
                }
                Err(error) => eprintln!("cannot render the failure: {error}"),
            }
            Ok(())
        }
    }
}

/// The session whose directory is `path`: a directory named for a session id.
fn session_dir_of(path: &Path) -> Result<SessionDir, ServeError> {
    let not_a_session = || ServeError::NotASession(path.to_path_buf());
    let id = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| SessionId::parse(name).ok())
        .ok_or_else(not_a_session)?;
    let state_dir = path.parent().ok_or_else(not_a_session)?;
    let dir = SessionDir::at(state_dir, id);
    if dir.exists() {
        Ok(dir)
    } else {
        Err(not_a_session())
    }
}

/// Takes the exclusive lock that says this supervisor is alive.
fn acquire_lock(dir: &SessionDir) -> Result<File, ServeError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.lock())
        .map_err(ServeError::Lock)?;
    for _ in 0..LOCK_ATTEMPTS {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(50)),
            Err(TryLockError::Error(error)) => return Err(ServeError::Lock(error)),
        }
    }
    Err(ServeError::Locked)
}

/// What a session's end leaves to report.
#[derive(Debug, Clone)]
struct Final {
    exit: ExitInfo,
    screen: ScreenInfo,
    events: EventsDigest,
    recording: Option<String>,
    fixture: FixtureChanges,
    residue: Vec<String>,
    cleanup: Cleanup,
}

/// A request served: the reply, and whether the session ended with it.
struct Handled {
    document: HarnessTui,
    ends: bool,
}

/// A running session.
struct Session {
    dir: SessionDir,
    config: Config,
    size: Size,
    live: Live,
    fixture: FixtureRoot,
    run_copy: Option<RunCopy>,
    scratch: Option<Scratch>,
    /// The directory the run copy and the scratch area are in, which this supervisor made.
    workspace: Workspace,
    /// The fixture before the first deletion; taken when the first deletion asks for it, since
    /// until then nothing can have changed it.
    baseline: Option<FixtureSnapshot>,
    /// The fixture-relative paths of the deletions that were confirmed.
    intended_deletions: Vec<String>,
    /// How many events earlier replies have reported.
    reported: usize,
    /// When the last command was served, or the session became ready.
    last_activity: Instant,
    /// What the end of the session left to report, once it has ended.
    ended: Option<Final>,
}

impl Drive for Session {
    fn live(&self) -> &Live {
        &self.live
    }

    fn live_mut(&mut self) -> &mut Live {
        &mut self.live
    }

    fn pump(&mut self) -> Result<(), RunError> {
        self.live.pump()
    }
}

fn failed(error: impl std::fmt::Display) -> TuiError {
    TuiError::new(TuiErrorKind::Failed, error.to_string())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

impl Session {
    /// Makes the workspace and the run copy, starts the program on it, and waits for its first
    /// frame, then announces the session.
    ///
    /// What a start that fails made goes: the program with the session that held it, and the
    /// workspace, which only this supervisor may remove (see [`Workspace`]).
    fn start(dir: &SessionDir, config: &Config, state: &mut State) -> Result<Self, TuiError> {
        let workspace =
            Workspace::create(&config.work_base, dir.id(), &config.nonce).map_err(|error| {
                failed(format!(
                    "cannot make a workspace in `{}`: {error}",
                    config.work_base.display()
                ))
            })?;
        Self::launch(dir, config, state, &workspace).inspect_err(|_| {
            if let Err(problem) = workspace.remove() {
                eprintln!("{problem}");
            }
        })
    }

    /// Generates the run copy in `workspace`, starts the program on it with the session's mark in
    /// its environment, records what the system says about the program, and waits for the first
    /// frame.
    fn launch(
        dir: &SessionDir,
        config: &Config,
        state: &mut State,
        workspace: &Workspace,
    ) -> Result<Self, TuiError> {
        let drain_bytes_per_sec = drain_cap()?;
        let workspace_dir = workspace.path();
        let fixtures = Fixtures::bundled();
        let spec = fixtures
            .spec(&config.fixture)
            .map_err(|error| failed(format!("fixture `{}`: {error}", config.fixture)))?;
        if spec
            .parts
            .iter()
            .any(|part| matches!(part, Part::Volume(_)))
        {
            return Err(failed(format!(
                "the fixture `{}` needs a volume, which the driver never attaches",
                config.fixture
            )));
        }
        let run_copy = fixtures
            .run_copy(&config.fixture, &workspace_dir)
            .map_err(|error| {
                failed(format!(
                    "cannot generate the fixture `{}`: {error}",
                    config.fixture
                ))
            })?;
        let fixture = FixtureRoot::open(run_copy.root()).map_err(failed)?;
        state.root = Some(fixture.path().to_path_buf());
        dir.write_state(state).map_err(failed)?;

        let scratch = Scratch::create(&workspace_dir).map_err(failed)?;
        let size = config.terminal_size();
        let program = resolve_binary(&config.binary).map_err(failed)?;
        let mut env = isolated_env(&scratch, config.profile, true, None);
        env.push((NONCE_VARIABLE.into(), config.nonce.as_str().into()));
        let pty = PtySession::spawn(&SpawnSpec {
            program,
            args: vec![fixture.path().as_os_str().to_owned()],
            env,
            cwd: scratch.cwd(),
            cols: size.cols,
            rows: size.rows,
            drain_bytes_per_sec,
            recording: config.record.clone(),
            title: Some(format!(
                "excise tui session {} ({})",
                dir.id(),
                config.profile
            )),
        })
        .map_err(failed)?;
        // The program is recorded as soon as it exists, so that nothing later can lose it. What the
        // system says about it is recorded once it shows the session's mark, which a process that
        // is still starting does not (see `identity`).
        state.child_pid = Some(pty.pid());
        dir.write_state(state).map_err(failed)?;
        state.child = ProcessIdentity::once_marked(pty.pid(), &config.nonce, IDENTITY_WAIT);
        dir.write_state(state).map_err(failed)?;

        let mut session = Self {
            dir: dir.clone(),
            config: config.clone(),
            size,
            live: Live::new(pty, scratch.events()),
            fixture,
            run_copy: Some(run_copy),
            scratch: Some(scratch),
            workspace: workspace.clone(),
            baseline: None,
            intended_deletions: Vec::new(),
            reported: 0,
            last_activity: Instant::now(),
            ended: None,
        };
        session.wait_for_first_frame()?;

        state.ready = true;
        dir.write_state(state).map_err(failed)?;
        let document = session.opened();
        let bytes = serde_json::to_vec_pretty(&document).map_err(failed)?;
        layout::write_atomic(&dir.ready(), &bytes).map_err(failed)?;
        session.last_activity = Instant::now();
        Ok(session)
    }

    fn wait_for_first_frame(&mut self) -> Result<(), TuiError> {
        let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
        let waited = self
            .wait_until(deadline, |session| {
                let seq = session.live.first_frame_from(0)?;
                header_state(session.live.session.screen())
                    .is_some()
                    .then_some(seq)
            })
            .map_err(failed)?;
        let shown = match waited {
            Waited::Ready(seq) => self.catch_up(seq, deadline).map_err(failed)?,
            other => other.discard(),
        };
        match shown {
            Waited::Ready(()) => Ok(()),
            Waited::Exited => {
                let mut error = self.exited_error();
                error.message = format!(
                    "excise ended before it drew its first frame: {}",
                    error.message
                );
                Err(error)
            }
            Waited::TimedOut => {
                let mut error = TuiError::new(
                    TuiErrorKind::Timeout,
                    format!(
                        "excise drew no frame that reached the screen within {} s ({})",
                        FIRST_FRAME_TIMEOUT.as_secs(),
                        self.live.frames_summary()
                    ),
                );
                error.screen = Some(Box::new(screen_info(&self.live.session)));
                Err(error)
            }
        }
    }

    /// The document that answers `open`.
    fn opened(&mut self) -> HarnessTui {
        let result = OpenResult {
            fixture: self.config.fixture.clone(),
            profile: self.config.profile,
            size: self.size,
            root: self.fixture.path().display().to_string(),
            session_dir: self.dir.path().display().to_string(),
            pid: u64::from(self.live.session.pid()),
            supervisor_pid: u64::from(process::id()),
            idle_timeout_ms: self.config.idle_timeout_ms,
            recording: self
                .config
                .record
                .as_ref()
                .map(|path| path.display().to_string()),
            screen: screen_info(&self.live.session),
            events: self.take_events(),
        };
        self.success(TuiResult::Open(result))
    }

    /// Serves requests until the session ends.
    fn run(mut self) {
        let idle_timeout = Duration::from_millis(self.config.idle_timeout_ms);
        loop {
            if let Err(error) = self.pump() {
                eprintln!("the session cannot go on: {error}");
                return self.end_without_a_command();
            }
            match self.dir.take_request() {
                Ok(Some(taken)) => {
                    let handled = match taken.request {
                        Ok(request) => self.handle(request),
                        Err(message) => Handled {
                            document: HarnessTui::failure(
                                None,
                                Some(self.id()),
                                TuiError::new(TuiErrorKind::Usage, message),
                            ),
                            ends: false,
                        },
                    };
                    if let Err(error) = self.dir.reply(&taken.name, &handled.document) {
                        eprintln!("cannot write the reply: {error}");
                    }
                    self.last_activity = Instant::now();
                    if handled.ends {
                        self.wait_for_the_reply_to_be_read(&taken.name);
                        return self.finish();
                    }
                    continue;
                }
                Ok(None) => {}
                Err(error) => {
                    eprintln!("cannot read the session's requests: {error}");
                    return self.end_without_a_command();
                }
            }
            if self.live.session.exit().is_some() || self.last_activity.elapsed() >= idle_timeout {
                return self.end_without_a_command();
            }
            let poll = match self.last_activity.elapsed() {
                idle if idle < Duration::from_secs(2) => Duration::from_millis(5),
                idle if idle < Duration::from_secs(60) => Duration::from_millis(25),
                _ => Duration::from_millis(100),
            };
            if let Err(error) = self.live.session.wait_activity(poll) {
                eprintln!("the session cannot go on: {error}");
                return self.end_without_a_command();
            }
        }
    }

    /// Ends a session nobody is waiting on: the program ended by itself, or the session was idle
    /// for its whole timeout. The program is closed the way `close` closes it.
    fn end_without_a_command(mut self) {
        let via = self.quit_the_way_a_user_does();
        self.finalize(via);
        self.finish();
    }

    /// Removes the session directory, unless something of the session is left to be found there.
    fn finish(self) {
        let clean = self
            .ended
            .as_ref()
            .is_some_and(|ended| ended.cleanup.removed);
        if clean {
            if let Err(error) = self.dir.remove() {
                eprintln!("cannot remove the session directory: {error}");
            }
        } else {
            eprintln!(
                "leaving the session directory: not everything of the session could be removed"
            );
        }
    }

    fn wait_for_the_reply_to_be_read(&self, name: &str) {
        let deadline = Instant::now() + REPLY_READ_TIMEOUT;
        while self.dir.reply_waiting(name) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn id(&self) -> String {
        self.dir.id().to_string()
    }

    fn success(&self, result: TuiResult) -> HarnessTui {
        HarnessTui::success(Some(self.id()), result)
    }

    fn failure(&self, command: TuiCommand, mut error: TuiError) -> HarnessTui {
        // A failed deletion says whether its confirmation key was sent. Only the protocol sends
        // it, and it says so when it did.
        if command == TuiCommand::Delete {
            error.confirmed.get_or_insert(false);
        }
        HarnessTui::failure(Some(command), Some(self.id()), error)
    }

    /// The events since the last reply that reported events, with the frames summarized.
    fn take_events(&mut self) -> EventsDigest {
        let events = self.live.events.events();
        let page = events_page(events, self.reported as u64);
        self.reported = events.len();
        page.digest()
    }

    fn latest_frame(&self) -> Option<FrameInfo> {
        self.live
            .events
            .events()
            .iter()
            .rev()
            .find_map(|event| match event.payload {
                Payload::Frame { seq, inputs, .. } => Some(FrameInfo { seq, inputs }),
                _ => None,
            })
    }

    /// Reads the program's output on until it has been quiet for [`KEYS_QUIET`], for at most
    /// [`KEYS_LIMIT`].
    fn read_on(&mut self) -> Result<(), RunError> {
        self.live.session.drain(KEYS_QUIET, KEYS_LIMIT)?;
        self.pump()
    }

    /// Whether `bytes` is the key that confirms the quit dialog the screen shows.
    fn confirms_the_quit(&self, bytes: &[u8]) -> bool {
        matches!(
            dialog_view(self.live.session.screen()),
            DialogView::Other(view)
                if quit_confirmation_key(&view).is_some_and(|key| bytes == [key])
        )
    }

    fn input_counts(&self) -> InputCounts {
        InputCounts {
            sent: self.live.inputs_sent(),
            consumed: self.live.latest_frame_inputs_sent(),
        }
    }

    fn handle(&mut self, request: Request) -> Handled {
        match request {
            Request::Screen => self.screen(),
            Request::Events { since } => self.events(since),
            Request::Keys { keys, timeout_ms } => {
                self.keys(&keys, Duration::from_millis(timeout_ms))
            }
            Request::Delete {
                name,
                kind,
                timeout_ms,
            } => self.delete(&name, kind, Duration::from_millis(timeout_ms)),
            Request::Close => self.close(),
        }
    }

    /// Reads what the program has written, and ends the command with the program's exit if it
    /// has ended.
    fn pump_or_end(&mut self, command: TuiCommand) -> Result<(), Box<Handled>> {
        if let Err(error) = self.pump() {
            return Err(Box::new(self.fatal(command, &error, None)));
        }
        if self.live.session.exit().is_some() {
            return Err(Box::new(self.ended(command, None)));
        }
        Ok(())
    }

    /// The end of a session whose program is gone, as a failure of `command`.
    fn ended(&mut self, command: TuiCommand, sent: Option<Vec<SentKey>>) -> Handled {
        let mut error = self.exited_error();
        error.sent = sent;
        Handled {
            document: self.failure(command, error),
            ends: true,
        }
    }

    /// The program is gone: finishes the session and says how it ended.
    fn exited_error(&mut self) -> TuiError {
        let ended = self.finalize(ExitVia::Exited);
        let mut error = TuiError::new(
            TuiErrorKind::ProgramExited,
            format!(
                "the program ended ({}); the session ended with it",
                describe_exit(&ended)
            ),
        );
        error.screen = Some(Box::new(ended.screen));
        error.exit = Some(ended.exit);
        error
    }

    /// A failure of the harness itself ends the session, since nothing it knows can be trusted.
    fn fatal(
        &mut self,
        command: TuiCommand,
        error: &RunError,
        sent: Option<Vec<SentKey>>,
    ) -> Handled {
        let ended = self.finalize(ExitVia::Killed);
        let mut report = failed(format!("{error}; the session was closed"));
        report.sent = sent;
        report.screen = Some(Box::new(ended.screen));
        report.exit = Some(ended.exit);
        Handled {
            document: self.failure(command, report),
            ends: true,
        }
    }

    fn screen(&mut self) -> Handled {
        if let Err(handled) = self.pump_or_end(TuiCommand::Screen) {
            return *handled;
        }
        Handled {
            document: self.success(TuiResult::Screen(ScreenResult {
                screen: screen_info(&self.live.session),
            })),
            ends: false,
        }
    }

    fn events(&mut self, since: Option<u64>) -> Handled {
        if let Err(handled) = self.pump_or_end(TuiCommand::Events) {
            return *handled;
        }
        Handled {
            document: self.success(TuiResult::Events(EventsResult {
                events: events_page(self.live.events.events(), since.unwrap_or(0)),
            })),
            ends: false,
        }
    }

    fn keys(&mut self, tokens: &[String], timeout: Duration) -> Handled {
        let command = TuiCommand::Keys;
        let inputs: Vec<Input> = match parse_keys(tokens) {
            Ok(inputs) => inputs,
            Err(message) => {
                return Handled {
                    document: self.failure(command, TuiError::new(TuiErrorKind::Usage, message)),
                    ends: false,
                };
            }
        };
        if let Err(handled) = self.pump_or_end(command) {
            return *handled;
        }
        if let Some(input) = inputs
            .iter()
            .find(|input| is_escape_then_confirmation(&input.bytes))
        {
            let mut error = TuiError::new(
                TuiErrorKind::Refused,
                format!(
                    "`{}` is the escape key and a key that could confirm a deletion dialog, sent \
                     as one input, and a program may read it as two: the escape can close a \
                     prompt that covers a deletion dialog, and the key then confirms the dialog \
                     it uncovers, with nobody having looked. Send `esc` and the key as two keys, \
                     so that the screen is read between them",
                    input.label
                ),
            );
            error.sent = Some(Vec::new());
            error.screen = Some(Box::new(screen_info(&self.live.session)));
            return Handled {
                document: self.failure(command, error),
                ends: false,
            };
        }
        let deadline = Instant::now() + timeout;

        let mut sent: Vec<SentKey> = Vec::new();
        // The first Backspace of this command, once one was sent: it asks for a deletion dialog.
        let mut asked_for_a_deletion: Option<String> = None;
        // Whether a key of this command confirmed the quit dialog: the program then ends.
        let mut quitting = false;
        for input in &inputs {
            if let Err(error) = self.pump() {
                return self.fatal(command, &error, Some(sent));
            }
            if self.live.session.exit().is_some() {
                break;
            }
            // What the program can read of this input, given every byte it was written before
            // it, by this command and by every earlier one.
            let reading = self.live.reading_of(&input.bytes);
            if reading.confirmation {
                // A dialog that Backspace asked for earlier in this very command has not been
                // read by anyone yet, whatever the screen shows now: the key waits for a command
                // of its own, after `screen` shows what is open.
                let verdict = match &asked_for_a_deletion {
                    Some(earlier) => Err(format!(
                        "`{}` could confirm a deletion dialog, and `{earlier}`, sent before it in \
                         this command, could have asked for one that nobody has read yet: send \
                         `{}` in a command of its own, after `screen` shows what is open",
                        input.label, input.label
                    )),
                    None => self.confirmation_is_safe(deadline),
                };
                if let Err(message) = verdict {
                    let mut error = TuiError::new(TuiErrorKind::Refused, message);
                    error.sent = Some(sent);
                    error.screen = Some(Box::new(screen_info(&self.live.session)));
                    return Handled {
                        document: self.failure(command, error),
                        ends: false,
                    };
                }
            }
            let confirms_quit = reading.confirmation && self.confirms_the_quit(&input.bytes);
            if let Err(error) = self.send_input(&input.bytes) {
                // The program may have ended while the key was being written: that is its
                // doing, not a failure of the driver.
                let wrote_to_a_closed_terminal = matches!(
                    error,
                    RunError::Pty(PtyError::Input(_) | PtyError::InputClosed)
                );
                let _ = self.pump();
                if wrote_to_a_closed_terminal && self.live.session.exit().is_some() {
                    break;
                }
                return self.fatal(command, &error, Some(sent));
            }
            if reading.request {
                asked_for_a_deletion.get_or_insert_with(|| input.label.clone());
            }
            if confirms_quit {
                quitting = true;
            }
            sent.push(SentKey {
                key: input.label.clone(),
                bytes: hex(&input.bytes),
            });
        }

        let waited = match self.wait_after_keys(quitting, deadline) {
            Ok(waited) => waited,
            Err(error) => return self.fatal(command, &error, Some(sent)),
        };
        self.keys_reply(command, sent, &waited)
    }

    /// What a `keys` command waits for once its keys are sent: the program to end, when a key
    /// confirmed the quit, and otherwise the frame that counts the keys.
    fn wait_after_keys(
        &mut self,
        quitting: bool,
        deadline: Instant,
    ) -> Result<Waited<()>, RunError> {
        if self.live.session.exit().is_some() {
            return Ok(Waited::Exited);
        }
        if quitting {
            // The quit was confirmed, so the program ends, and the command waits for that and
            // says how it ended.
            let ended = self.wait_until(Instant::now() + QUIT_EXIT_TIMEOUT, |session| {
                session.live.session.exit().map(|_| ())
            })?;
            return Ok(match ended {
                Waited::Ready(()) => Waited::Exited,
                other => other,
            });
        }
        self.settle_until(deadline)
    }

    /// The reply to `keys`, once its keys are sent and waited for.
    fn keys_reply(
        &mut self,
        command: TuiCommand,
        sent: Vec<SentKey>,
        waited: &Waited<()>,
    ) -> Handled {
        if matches!(waited, Waited::Exited) || self.live.session.exit().is_some() {
            return self.keys_ended(sent);
        }
        if let Err(error) = self.read_on() {
            return self.fatal(command, &error, Some(sent));
        }
        // The program can end while the last of its output is read: the reply says so, and the
        // session ends with it.
        if self.live.session.exit().is_some() {
            return self.keys_ended(sent);
        }
        let result = KeysResult {
            sent,
            settled: matches!(waited, Waited::Ready(())),
            frame: self.latest_frame(),
            inputs: self.input_counts(),
            screen: screen_info(&self.live.session),
            events: self.take_events(),
            exit: None,
        };
        Handled {
            document: self.success(TuiResult::Keys(result)),
            ends: false,
        }
    }

    /// The reply to `keys` when the program ended: how it ended. The session ends with it.
    fn keys_ended(&mut self, sent: Vec<SentKey>) -> Handled {
        let ended = self.finalize(ExitVia::Exited);
        let result = KeysResult {
            sent,
            settled: false,
            frame: self.latest_frame(),
            inputs: self.input_counts(),
            screen: ended.screen,
            events: ended.events,
            exit: Some(ended.exit),
        };
        Handled {
            document: self.success(TuiResult::Keys(result)),
            ends: true,
        }
    }

    /// Whether a key that could confirm a deletion may be sent now. `Err` says why not.
    ///
    /// It may only if the program marks its frames and answers input barrier requests, and its
    /// terminal's screen is exact: nothing else says that the screen shows what the program has
    /// read ([`Live::deletion_refusal`]). Then the program is asked for a barrier
    /// ([`Drive::barrier`]), written behind every earlier key: once it is answered, and the screen
    /// shows the frame that answers it, the program has read all of them, however the terminal cut
    /// their bytes into events, and the screen shows what they did. No deletion dialog may be
    /// shown then. A dialog opens on an input and never by itself, and nothing is queued behind
    /// the barrier, so a dialog that the screen does not show is not about to open. Otherwise the
    /// key is refused, whatever the clock says.
    fn confirmation_is_safe(&mut self, deadline: Instant) -> Result<(), String> {
        self.pump().map_err(|error| error.to_string())?;
        if let Some(reason) = self.live.deletion_refusal() {
            return Err(reason);
        }
        if self.live.holds_an_escape_sequence() {
            return Err(
                "an escape sequence that an earlier key began and nothing has finished is still \
                 open (`alt+[` and `alt+O` begin one): the program joins the bytes of such a \
                 sequence across writes, so the next key can finish it as a key that confirms a \
                 deletion (`ESC [ 1 2 1 u` is `y`), and no barrier can be written behind it, \
                 because the program takes the barrier for a part of the sequence and never \
                 answers it. The driver sends no key behind it: `close` the session and open \
                 another"
                    .to_owned(),
            );
        }
        match self.barrier(deadline).map_err(|error| error.to_string())? {
            Waited::Ready(()) => {}
            Waited::Exited => {
                return Err(
                    "the program ended before it answered the input barrier that comes before a \
                     key that could confirm a deletion"
                        .to_owned(),
                );
            }
            Waited::TimedOut => {
                return Err(format!(
                    "the program has not said that it read the keys before this one, and one of \
                     them may still open a deletion dialog that the screen does not show yet \
                     ({}): the driver asked with an input barrier, and the answer did not come, \
                     and show on the screen, in time; read `screen`, then send this key again",
                    self.live.frames_summary()
                ));
            }
        }
        if deletion_dialog_visible(self.live.session.screen()) {
            return Err(
                "a deletion dialog is open, so the driver sends no key that could confirm it \
                 (`y`, `enter`): use `delete --name <PATH> --kind <KIND>` to delete an entry, or \
                 `esc` to cancel the dialog"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// Closes whatever a command left open: a dialog, or the filter prompt. Best effort. What is
    /// open is read from a screen that shows what every key sent so far did: after a barrier where
    /// the program answers one, and otherwise from the latest frame it has drawn.
    fn restore_quiet(&mut self) {
        for _ in 0..2 {
            if self.pump().is_err() || self.live.session.exit().is_some() {
                return;
            }
            let deadline = Instant::now() + DISMISS_TIMEOUT;
            let _ = if self.live.input_barrier() {
                self.barrier(deadline)
            } else {
                self.catch_up_to_latest(deadline)
            };
            let screen = self.live.session.screen();
            if matches!(dialog_view(screen), DialogView::None) && filter_prompt(screen).is_none() {
                return;
            }
            if self.send_input(&[0x1b]).is_err() {
                return;
            }
            let _ = self.settle_until(Instant::now() + DISMISS_TIMEOUT);
        }
    }

    fn delete(&mut self, name: &str, kind: EntryKind, timeout: Duration) -> Handled {
        let command = TuiCommand::Delete;
        if let Err(handled) = self.pump_or_end(command) {
            return *handled;
        }
        match self.delete_entry(name, kind, timeout) {
            Ok(result) => Handled {
                document: self.success(TuiResult::Delete(result)),
                ends: false,
            },
            Err(error) => {
                let ends = error.exit.is_some();
                Handled {
                    document: self.failure(command, error),
                    ends,
                }
            }
        }
    }

    /// Refuses a name that cannot be the entry a deletion is for: it is not a fixture-relative
    /// path, it is the ownership marker or lies inside it (the marker is what makes the root a
    /// fixture, so no deletion may name it), or it holds glob syntax that the filter would
    /// interpret. No key has been sent when this refuses.
    fn check_target_name(name: &str) -> Result<(), TuiError> {
        check_fixture_relative_path(name).map_err(|violation| {
            TuiError::new(
                TuiErrorKind::Usage,
                format!("`{name}` is not a fixture-relative path: {violation}"),
            )
        })?;
        if is_marker_path(name) {
            return Err(TuiError::new(
                TuiErrorKind::Refused,
                format!(
                    "`{name}` is the ownership marker of the fixture, or lies inside it: the marker \
                     is what makes the root a fixture, so no deletion may name it; no key was sent"
                ),
            ));
        }
        if name.contains(['*', '?', '[', '{']) {
            return Err(TuiError::new(
                TuiErrorKind::Refused,
                format!(
                    "the name {name:?} contains glob syntax, which the filter would interpret \
                     instead of matching literally"
                ),
            ));
        }
        Ok(())
    }

    /// The protocol of the scenario `delete` step, for the entry `name` of the fixture.
    fn delete_entry(
        &mut self,
        name: &str,
        kind: EntryKind,
        timeout: Duration,
    ) -> Result<DeleteResult, TuiError> {
        let deadline = Instant::now() + timeout;
        Self::check_target_name(name)?;
        let leaf = name.rsplit('/').next().unwrap_or(name);
        let leaf_keys = encode_text(leaf)
            .map_err(|error| TuiError::new(TuiErrorKind::Usage, error.to_string()))?;

        // A program that does not mark its frames, and a terminal whose screen is not exact, are
        // refused before any key is sent, not after the keys that select the entry: the protocol
        // would refuse them anyway, when it came to Backspace.
        self.pump().map_err(failed)?;
        if let Some(reason) = self.live.deletion_refusal() {
            return Err(TuiError::new(TuiErrorKind::Refused, reason));
        }

        self.ensure_quiet(deadline)?;
        self.target_must_exist(name, kind)?;
        self.target_must_be_in_view(name)?;
        self.wait_for_complete(deadline)?;
        let sentinels = self.sentinels(name)?;

        if let Err(error) = self.select_target(leaf, &leaf_keys, deadline) {
            let error = self.protocol_error(error, false);
            self.restore_quiet();
            return Err(self.with_screen(error));
        }

        let root = self.fixture.clone();
        let request = DeletionRequest {
            name: leaf,
            kind,
            fixture: &root,
            sentinels: &sentinels,
            relative: name,
            confirm_with: ConfirmKey::Y,
        };
        let inputs_before = self.live.inputs_sent();
        let confirmed = match self.confirm_deletion(&request, deadline) {
            Ok(confirmed) => confirmed,
            Err(error) => {
                let attempted =
                    confirmation_may_have_arrived(inputs_before, self.live.inputs_sent(), &error);
                let error = self.protocol_error(error, attempted);
                self.restore_quiet();
                return Err(self.with_screen(error));
            }
        };
        self.intended_deletions
            .push(confirmed.verified.relative.clone());
        let finished = self
            .wait_deletion_finished(confirmed.events_before, deadline)
            .map_err(|error| self.protocol_error(error, true))?;

        // The program rebuilds its map after a deletion. The command waits for that as long as its
        // own bound allows, so that the next command starts from a settled map; the deletion is
        // done and verified whether the map is rebuilt in time or not, so a map that is still
        // being rebuilt is reported as it is (`screen.header_state` says), not as a failure.
        let rebuilt = deadline;
        let settled =
            match self.wait_for_the_frame_after_the_deletion(confirmed.events_before, rebuilt) {
                Ok(()) => self.wait_for_complete(rebuilt),
                Err(error) => Err(error),
            };
        match settled {
            Err(mut error) if error.exit.is_some() => {
                error.confirmed = Some(true);
                return Err(error);
            }
            Ok(()) | Err(_) => {}
        }

        let fixture = self.fixture_changes();
        if !fixture.unexpected.is_empty() {
            let mut error = TuiError::new(
                TuiErrorKind::FixtureChanged,
                format!(
                    "the deletion of `{name}` changed more of the fixture than its target: {}",
                    summarize(&fixture.unexpected)
                ),
            );
            error.confirmed = Some(true);
            return Err(self.with_screen(error));
        }
        Ok(DeleteResult {
            name: name.to_owned(),
            kind,
            dialog: delete_dialog_info(&confirmed.dialog),
            sentinels_checked: sentinels.len() as u64,
            removed: finished.removed,
            failed: finished.failed,
            fixture,
            screen: screen_info(&self.live.session),
            events: self.take_events(),
        })
    }

    /// How many deletions the driver confirmed that the program has not reported finished.
    fn deletions_running(&self) -> usize {
        unfinished_deletions(self.intended_deletions.len(), self.live.events.events())
    }

    /// Waits for the deletions that earlier commands confirmed and did not see finish. The
    /// program can queue a deletion behind one that is running, so a new one that did not wait
    /// could take the finish of the old one for its own.
    fn wait_for_earlier_deletions(&mut self, deadline: Instant) -> Result<(), TuiError> {
        if self.deletions_running() == 0 {
            return Ok(());
        }
        let waited = self
            .wait_until(deadline, |session| {
                (session.deletions_running() == 0).then_some(())
            })
            .map_err(failed)?;
        match waited {
            Waited::Ready(()) => Ok(()),
            Waited::Exited => Err(self.exited_error()),
            Waited::TimedOut => Err(self.with_screen(TuiError::new(
                TuiErrorKind::Conflict,
                "an earlier deletion was confirmed and has not reported that it finished, so \
                 another is not started behind it: `screen` and `events` show how far it is; \
                 try again once it is done",
            ))),
        }
    }

    /// A deletion starts from the map: no dialog, no prompt, and nothing the program may still be
    /// about to show.
    fn ensure_quiet(&mut self, deadline: Instant) -> Result<(), TuiError> {
        self.wait_for_earlier_deletions(deadline)?;
        self.pump().map_err(failed)?;
        match self.barrier(deadline).map_err(failed)? {
            Waited::Ready(()) => {}
            Waited::Exited => return Err(self.exited_error()),
            Waited::TimedOut => {
                return Err(self.with_screen(TuiError::new(
                    TuiErrorKind::Conflict,
                    format!(
                        "the program has not said that it read every key sent so far, and one of \
                         them may still make it show a dialog that the screen does not show yet \
                         ({}): the driver asked with an input barrier, and the answer did not \
                         come, and show on the screen, in time; read `screen`, then try again",
                        self.live.frames_summary()
                    ),
                )));
            }
        }
        let screen = self.live.session.screen();
        let reason = if deletion_dialog_visible(screen) {
            Some("a deletion dialog is open: cancel it with `keys <SESSION> esc` first".to_owned())
        } else if let DialogView::Other(view) = dialog_view(screen) {
            Some(format!(
                "the `{}` dialog is open: close it with `keys <SESSION> esc` first",
                view.title
            ))
        } else if filter_prompt(screen).is_some() {
            Some("the filter prompt is open: close it with `keys <SESSION> esc` first".to_owned())
        } else {
            None
        };
        match reason {
            Some(reason) => Err(self.with_screen(TuiError::new(TuiErrorKind::Conflict, reason))),
            None => Ok(()),
        }
    }

    /// Waits for the first frame the program draws after the deletion that `events_before` events
    /// preceded finished, until the screen shows it, so that the screen read next shows what the
    /// finish did (the notice, and the header of a map that is being rebuilt) and not the screen
    /// before it: the event is written when the deletion finishes, and the program draws
    /// afterwards. Running out of time is not an error here; the program ending is.
    fn wait_for_the_frame_after_the_deletion(
        &mut self,
        events_before: usize,
        deadline: Instant,
    ) -> Result<(), TuiError> {
        let waited = self
            .wait_until(deadline, |session| {
                let events = session.live.events.events();
                let finished =
                    events.iter().skip(events_before).position(|event| {
                        matches!(event.payload, Payload::DeletionFinished { .. })
                    })? + events_before;
                session.live.first_frame_from(finished + 1)
            })
            .map_err(failed)?;
        match waited {
            Waited::Ready(seq) => match self.catch_up(seq, deadline).map_err(failed)? {
                Waited::Exited => Err(self.exited_error()),
                Waited::Ready(()) | Waited::TimedOut => Ok(()),
            },
            Waited::Exited => Err(self.exited_error()),
            Waited::TimedOut => Ok(()),
        }
    }

    /// Waits for the scan to complete: the header badge is the only valid completion signal.
    fn wait_for_complete(&mut self, deadline: Instant) -> Result<(), TuiError> {
        let waited = self
            .wait_until(deadline, |session| {
                header_state(session.live.session.screen())
                    .is_some_and(|state| state.is(ScanState::Complete))
                    .then_some(())
            })
            .map_err(failed)?;
        match waited {
            Waited::Ready(()) => Ok(()),
            Waited::Exited => Err(self.exited_error()),
            Waited::TimedOut => Err(self.with_screen(TuiError::new(
                TuiErrorKind::Conflict,
                "the scan has not completed: the header badge does not read COMPLETE, so there \
                 is no settled map to delete from",
            ))),
        }
    }

    /// The target is looked up on disk before anything is sent, without following links.
    fn target_must_exist(&self, name: &str, kind: EntryKind) -> Result<(), TuiError> {
        match self.fixture.lstat(name) {
            Ok(Some(metadata)) => {
                let is_folder = metadata.is_dir();
                if is_folder == (kind == EntryKind::Folder) {
                    Ok(())
                } else {
                    Err(TuiError::new(
                        TuiErrorKind::Refused,
                        format!(
                            "`{name}` is a {} on disk, but the request deletes a {kind}",
                            if is_folder { "folder" } else { "file" }
                        ),
                    ))
                }
            }
            Ok(None) => Err(TuiError::new(
                TuiErrorKind::Refused,
                format!("`{name}` does not exist in the fixture"),
            )),
            Err(error) => Err(failed(format!(
                "`{name}` cannot be looked up safely: {error}"
            ))),
        }
    }

    /// The entry must be one of the folder the program shows: the filter selects among that
    /// folder's entries, so an entry anywhere else can never be selected. This fails at once, and
    /// says so, instead of after a wait for a selection that cannot happen.
    ///
    /// When the header cannot say which folder is open (it is cut, or not drawn), the protocol
    /// decides, as it always did.
    fn target_must_be_in_view(&mut self, name: &str) -> Result<(), TuiError> {
        self.pump().map_err(failed)?;
        let Some(shown) = open_folder(self.live.session.screen(), self.fixture.path()) else {
            return Ok(());
        };
        let wanted = name.rsplit_once('/').map_or("", |(folder, _)| folder);
        if shown == wanted {
            return Ok(());
        }
        let describe = |folder: &str| {
            if folder.is_empty() {
                "the fixture's root".to_owned()
            } else {
                format!("`{folder}`")
            }
        };
        let message = format!(
            "`{name}` is not in the folder the session shows ({}): `delete` acts on the entries \
             of the open folder, so navigate to {} first (`keys <SESSION> esc` goes up a folder; \
             a folder opens when it is selected, with `/`, its name, and `enter`, and `enter` is \
             pressed again)",
            describe(&shown),
            describe(wanted)
        );
        Err(self.with_screen(TuiError::new(TuiErrorKind::NotInView, message)))
    }

    /// Makes the inspector show the entry `leaf` of the open folder: it already does after a
    /// selection the caller made, and otherwise the filter selects it.
    ///
    /// The program selects nothing when it is given the filter it already has. That is the state
    /// after a selection through the filter that a retry applies again, or that an earlier
    /// `delete` left behind when it failed, so the filter is cleared before the name is typed.
    fn select_target(
        &mut self,
        leaf: &str,
        leaf_keys: &[Vec<u8>],
        deadline: Instant,
    ) -> Result<(), ProtocolError> {
        self.pump()?;
        if matches!(
            inspector(self.live.session.screen()),
            Inspector::Item(item) if item.name == leaf
        ) {
            return Ok(());
        }
        self.clear_filter(deadline)?;
        self.select_entry(leaf, leaf_keys, deadline)
    }

    /// The entries that must survive a deletion of `name`, from the fixture before any deletion.
    fn sentinels(&mut self, name: &str) -> Result<Vec<String>, TuiError> {
        if self.baseline.is_none() {
            self.baseline = Some(
                FixtureSnapshot::take(self.fixture.path())
                    .map_err(|error| failed(format!("cannot record the fixture: {error}")))?,
            );
        }
        let sentinels = self
            .baseline
            .as_ref()
            .map(|baseline| sentinels_of(baseline, name, &self.intended_deletions))
            .unwrap_or_default();
        if sentinels.is_empty() {
            return Err(TuiError::new(
                TuiErrorKind::Refused,
                format!(
                    "there is no entry that must survive the deletion of `{name}` (nothing in \
                     the fixture's first two levels lies outside it), so nothing would show a \
                     deletion that went wrong"
                ),
            ));
        }
        Ok(sentinels)
    }

    /// The fixture now, against the fixture before any deletion.
    fn fixture_changes(&self) -> FixtureChanges {
        let Some(baseline) = &self.baseline else {
            return FixtureChanges {
                removed: 0,
                unexpected: Vec::new(),
            };
        };
        match FixtureSnapshot::take(self.fixture.path()) {
            Ok(after) => {
                let diff = baseline.diff(&after);
                FixtureChanges {
                    removed: diff.removed.len() as u64,
                    unexpected: diff.unexpected(&self.intended_deletions, &[]),
                }
            }
            Err(error) => FixtureChanges {
                removed: 0,
                unexpected: vec![format!("the fixture cannot be walked: {error}")],
            },
        }
    }

    /// A protocol that did not complete, as the failure of the command.
    fn protocol_error(&mut self, error: ProtocolError, confirmed: bool) -> TuiError {
        match error {
            ProtocolError::Run(error) => {
                let ended = self.finalize(ExitVia::Killed);
                let mut report = failed(format!("{error}; the session was closed"));
                report.exit = Some(ended.exit);
                report.screen = Some(Box::new(ended.screen));
                report.confirmed = Some(confirmed);
                report
            }
            ProtocolError::Unmet(unmet) => {
                let message = format!("expected {}; {}", unmet.expected, unmet.observed);
                let mut report = match unmet.waited {
                    Waited::Exited => {
                        let mut report = self.exited_error();
                        report.message = format!("{message}\n{}", report.message);
                        report
                    }
                    Waited::TimedOut => TuiError::new(
                        TuiErrorKind::Timeout,
                        format!("not within the time allowed: {message}"),
                    ),
                    Waited::Ready(()) => TuiError::new(
                        if unmet.cause == FailureCause::DeleteRefused {
                            TuiErrorKind::Refused
                        } else {
                            TuiErrorKind::Conflict
                        },
                        message,
                    ),
                };
                if report.screen.is_none() {
                    report.screen = Some(Box::new(screen_info(&self.live.session)));
                }
                report.confirmed = Some(confirmed);
                report
            }
        }
    }

    /// `error` with the screen as it is now, unless it already has one.
    fn with_screen(&mut self, mut error: TuiError) -> TuiError {
        if error.screen.is_none() {
            let _ = self.pump();
            error.screen = Some(Box::new(screen_info(&self.live.session)));
        }
        error
    }

    fn close(&mut self) -> Handled {
        let via = self.quit_the_way_a_user_does();
        let ended = self.finalize(via);
        Handled {
            document: self.success(TuiResult::Close(CloseResult {
                exit: ended.exit,
                screen: ended.screen,
                events: ended.events,
                recording: ended.recording,
                fixture: ended.fixture,
                residue: ended.residue,
                cleanup: ended.cleanup,
            })),
            ends: true,
        }
    }

    /// Quits the program the way a user does: closes what is open, presses `q`, and confirms the
    /// dialog with the key it offers. Returns how the program ended; a program that does not end
    /// within the bound is left for [`Session::finalize`] to kill.
    fn quit_the_way_a_user_does(&mut self) -> ExitVia {
        let _ = self.pump();
        if self.live.session.exit().is_some() {
            return ExitVia::Exited;
        }
        self.restore_quiet();
        let mut confirmed = false;
        if let Ok(view) = self.request_quit(Instant::now() + QUIT_PROMPT_TIMEOUT)
            && let Some(key) = quit_confirmation_key(&view)
            && self.send_input(&[key]).is_ok()
        {
            confirmed = true;
        }
        if confirmed {
            let deadline = Instant::now() + QUIT_EXIT_TIMEOUT;
            let waited =
                self.wait_until(deadline, |session| session.live.session.exit().map(|_| ()));
            if matches!(waited, Ok(Waited::Ready(()))) {
                return ExitVia::Quit;
            }
        } else {
            let _ = self.pump();
            if self.live.session.exit().is_some() {
                return ExitVia::Exited;
            }
        }
        ExitVia::Killed
    }

    /// Ends the session: kills the program if it still runs, reads the rest of its output, takes
    /// down everything there is to report, and removes the run copy and the scratch area.
    fn finalize(&mut self, via: ExitVia) -> Final {
        if let Some(ended) = &self.ended {
            return ended.clone();
        }
        let mut via = via;
        if self.live.session.exit().is_none() {
            self.live.session.kill();
            via = ExitVia::Killed;
        }
        let deadline = Instant::now() + EXIT_OUTPUT_LIMIT;
        while !self.live.session.finished() && Instant::now() < deadline {
            let _ = self.live.pump();
            let _ = self.live.session.wait_activity(Duration::from_millis(1));
        }
        let _ = self.live.pump();
        let _ = self.live.session.finish_recording();

        let exit = exit_info(&self.live.session, via).unwrap_or_else(|| {
            let modes = self.live.session.modes();
            ExitInfo {
                code: None,
                signal: None,
                via: ExitVia::Killed,
                terminal_restored: false,
                modes: modes_info(modes),
            }
        });
        let screen = screen_info(&self.live.session);
        let events = self.take_events();
        let fixture = self.fixture_changes();
        let residue = match self.scratch.as_ref().map(Scratch::residue) {
            Some(Ok(residue)) => residue,
            Some(Err(error)) => vec![format!("the scratch area cannot be read: {error}")],
            None => Vec::new(),
        };
        let cleanup = self.remove_workspace();
        let ended = Final {
            exit,
            screen,
            events,
            recording: self
                .config
                .record
                .as_ref()
                .map(|path| path.display().to_string()),
            fixture,
            residue,
            cleanup,
        };
        self.ended = Some(ended.clone());
        ended
    }

    /// Removes the run copy, the scratch area, and the workspace around them.
    fn remove_workspace(&mut self) -> Cleanup {
        let mut problems = Vec::new();
        if let Some(copy) = self.run_copy.take()
            && let Err(error) = copy.remove()
        {
            problems.push(format!("cannot remove the run copy: {error}"));
        }
        drop(self.scratch.take());
        if let Err(problem) = self.workspace.remove() {
            problems.push(problem);
        }
        Cleanup {
            removed: problems.is_empty(),
            problems,
        }
    }
}

/// Whether the key that confirms a deletion may have reached the program, when the deletion
/// protocol failed with `error` after `inputs_before` inputs had been sent and `inputs_after` had.
///
/// The confirmation is the protocol's second input, after the Backspace that opens the dialog.
/// Once it was attempted the program may have it, whatever came of the rest of the write (an
/// input is recorded after it is written, and recording can fail), and the deletion may be
/// running. Only a write that failed itself says that the key did not arrive.
fn confirmation_may_have_arrived(
    inputs_before: u64,
    inputs_after: u64,
    error: &ProtocolError,
) -> bool {
    inputs_after >= inputs_before + 2
        && !matches!(
            error,
            ProtocolError::Run(RunError::Pty(PtyError::Input(_) | PtyError::InputClosed))
        )
}

/// How many of the `confirmed` deletions the program has not reported finished in `events`: it
/// reports one `deletion_finished` for each, in the order they were confirmed.
fn unfinished_deletions(confirmed: usize, events: &[Event]) -> usize {
    let finished = events
        .iter()
        .filter(|event| matches!(event.payload, Payload::DeletionFinished { .. }))
        .count();
    confirmed.saturating_sub(finished)
}

/// How the program ended, in a few words.
fn describe_exit(ended: &Final) -> String {
    match (ended.exit.code, ended.exit.signal) {
        (_, Some(signal)) => format!("killed by signal {signal}"),
        (Some(code), None) => format!("exit code {code}"),
        (None, None) => "an unknown status".to_owned(),
    }
}

/// The first few of `messages`, and how many there are.
fn summarize(messages: &[String]) -> String {
    const SHOWN: usize = 5;

    let mut text = messages
        .iter()
        .take(SHOWN)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("; ");
    if messages.len() > SHOWN {
        let _ = write!(text, "; and {} more", messages.len() - SHOWN);
    }
    text
}

/// The sentinels of a deletion of `target`: the entries of the fixture's first
/// [`SENTINEL_DEPTH`] levels, in path order, that are neither `target`, nor inside it, nor above
/// it, and that no earlier deletion (`deleted`) removed. At most [`MAX_SENTINELS`].
fn sentinels_of(snapshot: &FixtureSnapshot, target: &str, deleted: &[String]) -> Vec<String> {
    let inside = |path: &str, folder: &str| {
        path == folder
            || path
                .strip_prefix(folder)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    let above = |path: &str| {
        target
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with('/'))
    };
    snapshot
        .paths()
        .filter(|path| path.matches('/').count() < SENTINEL_DEPTH)
        .filter(|path| !inside(path, target) && !above(path))
        .filter(|path| !deleted.iter().any(|gone| inside(path, gone)))
        .take(MAX_SENTINELS)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod scripted;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::live::Unmet;

    fn snapshot(paths: &[&str]) -> (tempfile::TempDir, FixtureSnapshot) {
        let root = tempfile::tempdir().expect("a fixture");
        for path in paths {
            let full = root.path().join(path);
            if path.ends_with('/') {
                std::fs::create_dir_all(&full).expect("a directory");
            } else {
                std::fs::create_dir_all(full.parent().expect("a parent")).expect("directories");
                std::fs::write(&full, b"x").expect("a file");
            }
        }
        let taken = FixtureSnapshot::take(root.path()).expect("a snapshot");
        (root, taken)
    }

    #[test]
    fn the_sentinels_of_a_deletion_are_what_lies_outside_its_target_in_the_first_two_levels() {
        let (_root, snapshot) = snapshot(&[
            "keep-a.bin",
            "keep-b/keep.txt",
            "keep-b/deeper/too-deep.txt",
            "victim/part00/blob.bin",
            "victim/part01/blob.bin",
        ]);

        assert_eq!(
            sentinels_of(&snapshot, "victim", &[]),
            ["keep-a.bin", "keep-b", "keep-b/deeper", "keep-b/keep.txt"]
        );
        // Below the target's own parent: its siblings count, the folders above it do not.
        assert_eq!(
            sentinels_of(&snapshot, "victim/part00", &[]),
            [
                "keep-a.bin",
                "keep-b",
                "keep-b/deeper",
                "keep-b/keep.txt",
                "victim/part01",
            ]
        );
        assert_eq!(
            sentinels_of(&snapshot, "keep-b/keep.txt", &[]),
            [
                "keep-a.bin",
                "keep-b/deeper",
                "victim",
                "victim/part00",
                "victim/part01"
            ]
        );
    }

    #[test]
    fn a_name_that_merely_starts_like_the_target_is_still_a_sentinel() {
        let (_root, snapshot) = snapshot(&["victim/a", "victim-2/a", "victims"]);

        assert_eq!(
            sentinels_of(&snapshot, "victim", &[]),
            ["victim-2", "victim-2/a", "victims"]
        );
    }

    #[test]
    fn a_fixture_with_nothing_beside_the_target_has_no_sentinels() {
        let (_root, snapshot) = snapshot(&["wide/a", "wide/b"]);

        assert_eq!(sentinels_of(&snapshot, "wide", &[]), Vec::<String>::new());
    }

    #[test]
    fn the_sentinels_are_bounded() {
        let paths: Vec<String> = (0..MAX_SENTINELS + 30)
            .map(|index| format!("file-{index:03}"))
            .collect();
        let borrowed: Vec<&str> = paths.iter().map(String::as_str).collect();
        let (_root, snapshot) = snapshot(&borrowed);

        assert_eq!(
            sentinels_of(&snapshot, "file-000", &[]).len(),
            MAX_SENTINELS
        );
    }

    #[test]
    fn what_an_earlier_deletion_removed_is_no_sentinel_of_a_later_one() {
        let (_root, snapshot) = snapshot(&["keep-a.bin", "keep-b/keep.txt", "victim/a"]);
        let gone = |paths: &[&str]| -> Vec<String> {
            paths.iter().map(|path| (*path).to_owned()).collect()
        };

        assert_eq!(
            sentinels_of(&snapshot, "victim", &gone(&["keep-a.bin"])),
            ["keep-b", "keep-b/keep.txt"]
        );
        // What lies inside a removed folder went with it.
        assert_eq!(
            sentinels_of(&snapshot, "victim", &gone(&["keep-b"])),
            ["keep-a.bin"]
        );
        // A name that merely starts like a removed entry is still there.
        assert_eq!(
            sentinels_of(&snapshot, "victim", &gone(&["keep"])),
            ["keep-a.bin", "keep-b", "keep-b/keep.txt"]
        );
    }

    #[test]
    fn bytes_are_written_as_lowercase_hexadecimal() {
        assert_eq!(hex(&[0x1b, b'[', b'B', 0x7f, 0x00]), "1b5b427f00");
        assert_eq!(hex(&[]), "");
    }

    #[test]
    fn only_the_first_few_problems_are_summarised() {
        let many: Vec<String> = (0..8).map(|index| format!("problem {index}")).collect();

        assert_eq!(
            summarize(&many),
            "problem 0; problem 1; problem 2; problem 3; problem 4; and 3 more"
        );
        assert_eq!(summarize(&many[..2]), "problem 0; problem 1");
    }

    #[test]
    fn the_confirmation_key_may_have_arrived_unless_its_own_write_failed() {
        let recording = ProtocolError::Run(RunError::Pty(PtyError::Recording {
            path: PathBuf::from("/tmp/session.cast"),
            source: std::io::Error::other("the disk is full"),
        }));
        let write = ProtocolError::Run(RunError::Pty(PtyError::Input(std::io::Error::other(
            "a broken pipe",
        ))));
        let closed = ProtocolError::Run(RunError::Pty(PtyError::InputClosed));
        let unmet = ProtocolError::Unmet(Unmet {
            waited: Waited::TimedOut,
            cause: FailureCause::Mismatch,
            expected: "a deletion dialog to open".to_owned(),
            observed: "the screen shows nothing".to_owned(),
        });

        // The key was written, and recording it failed: the program has it, and the deletion may
        // be running, so the failure must not say that nothing was confirmed.
        assert!(confirmation_may_have_arrived(3, 5, &recording));
        // The write itself failed: the key did not arrive.
        assert!(!confirmation_may_have_arrived(3, 5, &write));
        assert!(!confirmation_may_have_arrived(3, 5, &closed));
        // Only the Backspace was sent, or nothing: the confirmation was never attempted.
        assert!(!confirmation_may_have_arrived(3, 4, &recording));
        assert!(!confirmation_may_have_arrived(3, 3, &unmet));
    }

    #[test]
    fn a_deletion_is_running_until_the_program_reports_its_finish() {
        let event = |payload| Event {
            t_us: 0,
            observed: Instant::now(),
            payload,
        };
        let finished = || {
            event(Payload::DeletionFinished {
                removed: 1,
                failed: 0,
            })
        };
        let frame = || {
            event(Payload::Frame {
                seq: 1,
                inputs: 3,
                barriers: 0,
            })
        };

        assert_eq!(unfinished_deletions(0, &[frame()]), 0);
        assert_eq!(unfinished_deletions(1, &[frame()]), 1);
        assert_eq!(unfinished_deletions(2, &[finished(), frame()]), 1);
        assert_eq!(unfinished_deletions(2, &[finished(), finished()]), 0);
        assert_eq!(unfinished_deletions(1, &[finished(), finished()]), 0);
    }

    #[test]
    fn the_drain_cap_is_a_positive_whole_number_or_nothing() {
        assert_eq!(parse_drain_cap(None).expect("no cap"), None);
        assert_eq!(
            parse_drain_cap(Some(OsStr::new("4000"))).expect("a cap"),
            Some(4000)
        );
        // A test that asked for a slow terminal must never run on a fast one.
        for wrong in ["", "0", "-1", "1.5", "fast", "4000 "] {
            let error = parse_drain_cap(Some(OsStr::new(wrong)))
                .expect_err("a value that is no pace is an error");
            assert!(
                error.message.contains(DRAIN_CAP_VARIABLE),
                "{wrong:?}: {}",
                error.message
            );
        }
    }
}
