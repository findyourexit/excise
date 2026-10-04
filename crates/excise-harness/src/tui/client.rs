//! The commands: what `cargo xtask tui <command>` does, with the harness's own pieces.
//!
//! `open` makes a session directory, starts a supervisor behind it (a detached process, so the
//! session outlives the command), and waits for the supervisor to say the session is ready; `list`
//! looks at the session directories; every other command leaves a request for the session's
//! supervisor and waits for its reply. A command never touches a session's workspace or program:
//! only the supervisor does, and only stale-session cleaning does without one.

use std::{
    fs,
    io::{self, Read, Seek, SeekFrom},
    os::unix::process::CommandExt as _,
    process::{Child, Command as Process, Stdio},
    thread,
    time::{Duration, Instant, SystemTime},
};

use super::{
    command::{
        Command, MAX_COMMAND_TIMEOUT, MAX_IDLE_TIMEOUT, MIN_IDLE_TIMEOUT, OpenRequest, Options,
        Outcome, check_fixture, parse_size,
    },
    identity,
    keys::parse_keys,
    layout::{self, Config, SessionDir, SessionId},
    mailbox::Request,
    stale,
};
use crate::{
    report::{
        HarnessTui,
        tui::{ListResult, TuiCommand, TuiError, TuiErrorKind, TuiResult},
    },
    run_support::rfc3339,
    runner::{resolve_binary, work_base},
    scenario::check_fixture_relative_path,
};

/// How long `open` waits for a session to be ready: the longest a large fixture takes to
/// generate, and then some.
const OPEN_TIMEOUT: Duration = Duration::from_mins(15);
/// How often a command looks for the reply it waits for.
const REPLY_POLL: Duration = Duration::from_millis(5);
/// How often a command that waits for a reply asks whether the supervisor is still alive.
const LIVENESS_POLL: Duration = Duration::from_millis(250);
/// What a command that waits for a reply allows the supervisor beyond the bound the command
/// carries: reading the screen, taking events, and writing the reply.
const REPLY_SLACK: Duration = Duration::from_secs(30);
/// What `delete` allows beyond its bound: recording a large fixture before the first deletion
/// and comparing it afterwards.
const DELETE_SLACK: Duration = Duration::from_secs(300);
/// How long `close` waits for the reply that ends a session.
const CLOSE_BOUND: Duration = Duration::from_secs(60);
/// How long a command that ended a session waits for the supervisor to remove the session
/// directory.
const REMOVAL_WAIT: Duration = Duration::from_secs(15);
/// How long a supervisor that answered `open` with a failure, or never answered, has to end by
/// itself before it is killed.
const SUPERVISOR_END_WAIT: Duration = Duration::from_secs(15);
/// How long a supervisor that was killed has to be gone.
const SUPERVISOR_KILL_WAIT: Duration = Duration::from_secs(5);
/// How much of the supervisor's log an error quotes.
const LOG_TAIL_BYTES: u64 = 2000;

/// Runs `command`.
#[must_use]
pub fn execute(options: &Options, command: &Command) -> Outcome {
    if let Err(error) = validate(command) {
        return Outcome::failure(Some(command.name()), None, error);
    }
    match command {
        Command::Open(request) => open(options, request),
        Command::List => list(options),
        Command::Keys {
            session,
            keys,
            timeout,
        } => relay(
            options,
            TuiCommand::Keys,
            session,
            &Request::Keys {
                keys: keys.clone(),
                timeout_ms: millis(*timeout),
            },
            *timeout + REPLY_SLACK,
        ),
        Command::Delete {
            session,
            name,
            kind,
            timeout,
        } => relay(
            options,
            TuiCommand::Delete,
            session,
            &Request::Delete {
                name: name.clone(),
                kind: *kind,
                timeout_ms: millis(*timeout),
            },
            *timeout + DELETE_SLACK,
        ),
        Command::Screen { session } => relay(
            options,
            TuiCommand::Screen,
            session,
            &Request::Screen,
            REPLY_SLACK,
        ),
        Command::Events { session, since } => relay(
            options,
            TuiCommand::Events,
            session,
            &Request::Events { since: *since },
            REPLY_SLACK,
        ),
        Command::Close { session } => relay(
            options,
            TuiCommand::Close,
            session,
            &Request::Close,
            CLOSE_BOUND,
        ),
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn usage(message: impl Into<String>) -> TuiError {
    TuiError::new(TuiErrorKind::Usage, message)
}

fn failed(message: impl std::fmt::Display) -> TuiError {
    TuiError::new(TuiErrorKind::Failed, message.to_string())
}

/// Everything about a command that can be checked without a session. Nothing is sent, started,
/// or made when it is not valid.
fn validate(command: &Command) -> Result<(), TuiError> {
    let bound = |timeout: &Duration| {
        if timeout.is_zero() || *timeout > MAX_COMMAND_TIMEOUT {
            Err(usage(format!(
                "a timeout must be more than zero and at most {} s",
                MAX_COMMAND_TIMEOUT.as_secs()
            )))
        } else {
            Ok(())
        }
    };
    match command {
        Command::Open(request) => {
            if request.idle_timeout < MIN_IDLE_TIMEOUT || request.idle_timeout > MAX_IDLE_TIMEOUT {
                return Err(usage(format!(
                    "the idle timeout must be between {} s and {} h",
                    MIN_IDLE_TIMEOUT.as_secs(),
                    MAX_IDLE_TIMEOUT.as_secs() / 3600
                )));
            }
            parse_size(&format!("{}x{}", request.size.0, request.size.1)).map_err(usage)?;
            Ok(())
        }
        Command::Keys { keys, timeout, .. } => {
            bound(timeout)?;
            parse_keys(keys).map(|_| ()).map_err(usage)
        }
        Command::Delete { name, timeout, .. } => {
            bound(timeout)?;
            check_fixture_relative_path(name).map_err(|violation| {
                usage(format!(
                    "`{name}` is not a fixture-relative path: {violation}"
                ))
            })?;
            if name.contains(['*', '?', '[', '{']) {
                return Err(usage(format!(
                    "the name {name:?} contains glob syntax, which the filter would interpret \
                     instead of matching literally"
                )));
            }
            Ok(())
        }
        Command::Screen { .. } | Command::Events { .. } | Command::Close { .. } | Command::List => {
            Ok(())
        }
    }
}

/// `open`: makes the session directory, starts a supervisor, and waits for its answer.
fn open(options: &Options, request: &OpenRequest) -> Outcome {
    let fail = |error: TuiError| Outcome::failure(Some(TuiCommand::Open), None, error);

    if let Err(error) = check_fixture(&request.fixture) {
        return fail(error);
    }
    let binary = match resolve_binary(&request.binary) {
        Ok(binary) => binary,
        Err(error) => return fail(failed(error)),
    };
    let work_base = work_base();
    if let Err(error) = fs::create_dir_all(&work_base) {
        return fail(failed(format!(
            "cannot make the work directory `{}`: {error}",
            work_base.display()
        )));
    }

    if let Err(error) = layout::ensure_state_dir(&options.state_dir) {
        return fail(failed(format!(
            "cannot make the session directory `{}`: {error}",
            options.state_dir.display()
        )));
    }
    match stale::sweep(&options.state_dir) {
        Ok(swept) if !swept.is_empty() => eprintln!(
            "cleaned {} stale session(s): {}",
            swept.len(),
            swept
                .iter()
                .map(|session| session.session.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Ok(_) => {}
        Err(error) => eprintln!("cannot look for stale sessions: {error}"),
    }

    let nonce = match identity::new_nonce() {
        Ok(nonce) => nonce,
        Err(error) => {
            return fail(failed(format!(
                "cannot make a mark for the session: {error}"
            )));
        }
    };
    let dir = match SessionDir::create(&options.state_dir) {
        Ok(dir) => dir,
        Err(error) => return fail(failed(format!("cannot make a session directory: {error}"))),
    };
    let config = Config {
        session: dir.id().to_string(),
        fixture: request.fixture.clone(),
        profile: request.profile,
        cols: request.size.0,
        rows: request.size.1,
        record: request
            .record
            .then(|| layout::recording_path(&options.state_dir, dir.id())),
        idle_timeout_ms: millis(request.idle_timeout),
        binary,
        work_base: work_base.canonicalize().unwrap_or(work_base),
        started_at: rfc3339(SystemTime::now()),
        nonce,
    };
    if let Err(error) = dir.write_config(&config) {
        let _ = dir.remove();
        return fail(failed(format!(
            "cannot write the session's configuration: {error}"
        )));
    }

    let mut child = match start_supervisor(options, &dir) {
        Ok(child) => child,
        Err(error) => {
            let _ = dir.remove();
            return fail(failed(format!(
                "cannot start `{}`: {error}",
                options.launcher.program.display()
            )));
        }
    };
    let answer = await_open(&dir, &mut child, OPEN_TIMEOUT);
    match answer {
        Ok(document) => {
            if !document.ok {
                // A session that failed to start is not left behind.
                end_failed_start(&mut child, &dir, SUPERVISOR_END_WAIT);
            }
            Outcome { document }
        }
        Err(error) => {
            // The supervisor ended without a word, its answer could not be read, or it was still
            // starting after the longest an open may take. None of those is a session, so what
            // is left of it goes, the supervisor included.
            end_failed_start(&mut child, &dir, SUPERVISOR_END_WAIT);
            Outcome::failure(Some(TuiCommand::Open), Some(dir.id().to_string()), error)
        }
    }
}

/// Starts the supervisor of `dir` as a process of its own: no terminal, no input, and a process
/// group of its own, so that what ends the command (a terminal closing, a tool killing the
/// command's process group on a timeout) does not end the session. Its standard error is the
/// session's log.
fn start_supervisor(options: &Options, dir: &SessionDir) -> io::Result<Child> {
    let log = fs::File::create(dir.log())?;
    Process::new(&options.launcher.program)
        .args(&options.launcher.args)
        .arg(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .process_group(0)
        .spawn()
}

/// Waits up to `limit` for the supervisor to answer `open`: `ready.json`, `failed.json`, or its
/// end.
fn await_open(
    dir: &SessionDir,
    child: &mut Child,
    limit: Duration,
) -> Result<HarnessTui, TuiError> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(document) = read_answer(dir)? {
            return Ok(document);
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // It may have written its answer just before it ended.
                if let Some(document) = read_answer(dir)? {
                    return Ok(document);
                }
                return Err(failed(format!(
                    "the supervisor ended ({status}) before the session was ready{}",
                    log_tail(dir)
                )));
            }
            Ok(None) => {}
            Err(error) => return Err(failed(error)),
        }
        if Instant::now() >= deadline {
            return Err(TuiError::new(
                TuiErrorKind::Timeout,
                format!(
                    "the session `{}` was not ready within {} s, so it was stopped and removed",
                    dir.id(),
                    limit.as_secs()
                ),
            ));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// The supervisor's answer to `open`, if it has given one.
fn read_answer(dir: &SessionDir) -> Result<Option<HarnessTui>, TuiError> {
    for path in [dir.ready(), dir.failed()] {
        match fs::read(&path) {
            Ok(bytes) => {
                return serde_json::from_slice(&bytes).map(Some).map_err(|error| {
                    failed(format!(
                        "the supervisor's answer `{}` is not a harness-tui document: {error}",
                        path.display()
                    ))
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(failed(error)),
        }
    }
    Ok(None)
}

/// The end of the supervisor's log, quoted in an error: what it said before it ended.
fn log_tail(dir: &SessionDir) -> String {
    let read = || -> io::Result<String> {
        let mut file = fs::File::open(dir.log())?;
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(LOG_TAIL_BYTES)))?;
        let mut text = String::new();
        file.take(LOG_TAIL_BYTES).read_to_string(&mut text)?;
        Ok(text)
    };
    match read() {
        Ok(text) if !text.trim().is_empty() => format!("; its log says:\n{}", text.trim_end()),
        _ => String::new(),
    }
}

/// Ends what a session that failed to start left. The supervisor is given `wait` to end by
/// itself and is killed if it does not, so that no wait here is unbounded and nothing is left
/// running. Then the session is cleaned the way a stale one is, which also kills a program that
/// survived. A supervisor that is known to be gone leaves no grace to wait out, however young the
/// session is.
fn end_failed_start(child: &mut Child, dir: &SessionDir, wait: Duration) {
    let ended = reaped(child, wait) || {
        let _ = child.kill();
        reaped(child, SUPERVISOR_KILL_WAIT)
    };
    let report = if ended {
        Some(stale::clean(dir))
    } else {
        eprintln!(
            "the supervisor of session {} did not end after it was killed",
            dir.id()
        );
        stale::sweep_one(dir)
    };
    if let Some(report) = report {
        for problem in &report.problems {
            eprintln!("session {}: {problem}", report.session);
        }
    }
}

/// Waits up to `limit` for `child` to end, and reaps it. `true` when it has ended.
fn reaped(child: &mut Child, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Ok(None) | Err(_) => return false,
        }
    }
}

/// `list`: cleans the stale sessions and lists the live ones.
fn list(options: &Options) -> Outcome {
    let stale = match stale::sweep(&options.state_dir) {
        Ok(stale) => stale,
        Err(error) => {
            return Outcome::failure(
                Some(TuiCommand::List),
                None,
                failed(format!(
                    "cannot look at `{}`: {error}",
                    options.state_dir.display()
                )),
            );
        }
    };
    let sessions = match stale::running(&options.state_dir) {
        Ok(sessions) => sessions,
        Err(error) => {
            return Outcome::failure(
                Some(TuiCommand::List),
                None,
                failed(format!(
                    "cannot look at `{}`: {error}",
                    options.state_dir.display()
                )),
            );
        }
    };
    Outcome {
        document: HarnessTui::success(None, TuiResult::List(ListResult { sessions, stale })),
    }
}

/// Leaves `request` for the supervisor of `session` and returns its reply.
fn relay(
    options: &Options,
    command: TuiCommand,
    session: &str,
    request: &Request,
    bound: Duration,
) -> Outcome {
    let id = match SessionId::parse(session) {
        Ok(id) => id,
        Err(message) => return Outcome::failure(Some(command), None, usage(message)),
    };
    let named = Some(id.to_string());
    let fail = |error: TuiError| Outcome::failure(Some(command), named.clone(), error);
    let dir = SessionDir::at(&options.state_dir, id);
    // The sessions of a state directory that is a link are not the driver's: a request sent
    // through it goes to whatever the link leads to, which a sweep may then clean.
    match layout::check_state_dir(&options.state_dir) {
        Ok(true) => {}
        Ok(false) => return fail(no_such_session(&dir)),
        Err(error) => {
            return fail(failed(format!(
                "cannot look at `{}`: {error}",
                options.state_dir.display()
            )));
        }
    }
    if !dir.exists() {
        return fail(no_such_session(&dir));
    }
    if let Some(report) = stale::sweep_one(&dir) {
        return fail(TuiError::new(
            TuiErrorKind::SessionLost,
            format!(
                "the session's supervisor is gone, so the session was stale; it has been cleaned{}",
                if report.problems.is_empty() {
                    String::new()
                } else {
                    format!(" except for: {}", report.problems.join("; "))
                }
            ),
        ));
    }
    let name = match dir.submit(request) {
        Ok(name) => name,
        Err(error) => return fail(failed(format!("cannot leave the request: {error}"))),
    };
    let mut document = match await_reply(&dir, &name, bound) {
        Ok(document) => document,
        Err(error) => return fail(error),
    };
    if ended(&document) && !wait_for_removal(&dir) {
        let problem = format!(
            "the session directory `{}` was still there {} s after the reply; `list` cleans it",
            dir.path().display(),
            REMOVAL_WAIT.as_secs()
        );
        document = with_cleanup_problem(document, &problem);
    }
    Outcome { document }
}

/// The last reply of a session whose directory was still there after the wait, with that said.
/// `close` has a place for it, in its cleanup. A reply that has none (`keys` that quit the
/// program, a failure that ended the session) says so in the way it can: a failure carries the
/// exit and the screen it had, so that nothing is lost, and a session that did not clean up is
/// never reported as one that did.
fn with_cleanup_problem(mut document: HarnessTui, problem: &str) -> HarnessTui {
    if let Some(TuiResult::Close(close)) = &mut document.result {
        close.cleanup.removed = false;
        close.cleanup.problems.push(problem.to_owned());
        return document;
    }
    if let Some(error) = &mut document.error {
        error.message = format!("{}; {problem}", error.message);
        return document;
    }
    if let Some(TuiResult::Keys(keys)) = document.result {
        let mut error = failed(format!(
            "the program ended, and the session with it, but {problem}"
        ));
        error.sent = Some(keys.sent);
        error.screen = Some(Box::new(keys.screen));
        error.exit = keys.exit;
        return HarnessTui::failure(document.command, document.session, error);
    }
    document
}

fn no_such_session(dir: &SessionDir) -> TuiError {
    TuiError::new(
        TuiErrorKind::NoSuchSession,
        format!(
            "there is no session `{}`: it never existed, or it has ended (`close`, its program \
             quit, or it was idle for its timeout); `cargo xtask tui list` shows the open sessions",
            dir.id()
        ),
    )
}

/// Waits for the reply to the request called `name`.
fn await_reply(dir: &SessionDir, name: &str, bound: Duration) -> Result<HarnessTui, TuiError> {
    let deadline = Instant::now() + bound;
    let mut next_probe = Instant::now() + LIVENESS_POLL;
    loop {
        match dir.take_reply(name) {
            Ok(Some(document)) => return Ok(document),
            Ok(None) => {}
            Err(error) => return Err(failed(error)),
        }
        let now = Instant::now();
        if now >= deadline {
            return timed_out(dir, name, bound);
        }
        if now >= next_probe {
            next_probe = now + LIVENESS_POLL;
            if !dir.exists() || stale::is_gone(dir) {
                // The reply may have been written just before the supervisor ended.
                if let Ok(Some(document)) = dir.take_reply(name) {
                    return Ok(document);
                }
                return Err(TuiError::new(
                    TuiErrorKind::SessionLost,
                    format!(
                        "the session's supervisor ended before it replied{}; `list` cleans what \
                         is left of the session",
                        log_tail(dir)
                    ),
                ));
            }
        }
        thread::sleep(REPLY_POLL);
    }
}

/// What a command does when no reply came in time. It withdraws its request if the supervisor has
/// not taken it, so that nothing runs after the command reported a timeout, and says which of the
/// two happened.
fn timed_out(dir: &SessionDir, name: &str, bound: Duration) -> Result<HarnessTui, TuiError> {
    if dir.withdraw(name) {
        return Err(TuiError::new(
            TuiErrorKind::Timeout,
            format!(
                "no reply within {} s: the supervisor was still busy with an earlier command, so \
                 this one was withdrawn before it ran; send it again when `screen` answers",
                bound.as_secs()
            ),
        ));
    }
    // The supervisor took the request, so it may have answered while this decided.
    if let Ok(Some(document)) = dir.take_reply(name) {
        return Ok(document);
    }
    Err(TuiError::new(
        TuiErrorKind::Timeout,
        format!(
            "no reply within {} s: the supervisor took the request and may still be serving it; \
             `screen` shows where the session is",
            bound.as_secs()
        ),
    ))
}

/// Whether a reply is the last of its session: the session ended with it.
fn ended(document: &HarnessTui) -> bool {
    match (&document.result, &document.error) {
        (Some(TuiResult::Close(_)), _) => true,
        (Some(TuiResult::Keys(keys)), _) => keys.exit.is_some(),
        (_, Some(error)) => error.exit.is_some(),
        _ => false,
    }
}

/// Waits for the supervisor to remove the session directory after its last reply.
fn wait_for_removal(dir: &SessionDir) -> bool {
    let deadline = Instant::now() + REMOVAL_WAIT;
    while dir.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    !dir.exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        report::tui::{
            Cleanup, CloseResult, EventsDigest, ExitInfo, ExitVia, FixtureChanges, KeysResult,
            Modes, ScreenInfo,
        },
        tui::command::{DEFAULT_IDLE_TIMEOUT, DEFAULT_KEYS_TIMEOUT},
    };

    fn modes() -> Modes {
        Modes {
            alternate_screen: false,
            cursor_visible: true,
            echo: Some(true),
            icanon: Some(true),
        }
    }

    fn screen() -> ScreenInfo {
        ScreenInfo {
            size: crate::report::tui::Size { cols: 80, rows: 24 },
            cursor: crate::report::tui::Cursor { row: 0, col: 0 },
            modes: modes(),
            header_state: None,
            selected: None,
            filter: None,
            dialog: None,
            boxes: Vec::new(),
            rows: Vec::new(),
        }
    }

    fn exit() -> ExitInfo {
        ExitInfo {
            code: Some(0),
            signal: None,
            via: ExitVia::Exited,
            terminal_restored: true,
            modes: modes(),
        }
    }

    fn page() -> EventsDigest {
        EventsDigest {
            since: 0,
            next: 0,
            frames: None,
            records: Vec::new(),
        }
    }

    #[test]
    fn a_reply_that_ends_the_session_is_recognised() {
        let keys = |exit: Option<ExitInfo>| {
            HarnessTui::success(
                Some("0123abcd".to_owned()),
                TuiResult::Keys(KeysResult {
                    sent: Vec::new(),
                    settled: true,
                    frame: None,
                    inputs: crate::report::tui::InputCounts {
                        sent: 0,
                        consumed: 0,
                    },
                    screen: screen(),
                    events: page(),
                    exit,
                }),
            )
        };
        let close = HarnessTui::success(
            Some("0123abcd".to_owned()),
            TuiResult::Close(CloseResult {
                exit: exit(),
                screen: screen(),
                events: page(),
                recording: None,
                fixture: FixtureChanges {
                    removed: 0,
                    unexpected: Vec::new(),
                },
                residue: Vec::new(),
                cleanup: Cleanup {
                    removed: true,
                    problems: Vec::new(),
                },
            }),
        );
        let program_exited = {
            let mut error = TuiError::new(TuiErrorKind::ProgramExited, "it ended");
            error.exit = Some(exit());
            HarnessTui::failure(Some(TuiCommand::Delete), Some("0123abcd".to_owned()), error)
        };
        let refused = HarnessTui::failure(
            Some(TuiCommand::Keys),
            Some("0123abcd".to_owned()),
            TuiError::new(TuiErrorKind::Refused, "no"),
        );

        assert!(!ended(&keys(None)));
        assert!(ended(&keys(Some(exit()))));
        assert!(ended(&close));
        assert!(ended(&program_exited));
        assert!(!ended(&refused));
    }

    #[test]
    fn a_session_that_did_not_clean_up_is_never_reported_as_one_that_did() {
        let problem = "the session directory `/x` was still there 15 s after the reply";
        let named = || Some("0123abcd".to_owned());

        // `keys` that made the program quit has no cleanup to say it in: it becomes a failure
        // that keeps the exit, the screen, and the keys.
        let quit = HarnessTui::success(
            named(),
            TuiResult::Keys(KeysResult {
                sent: vec![crate::report::tui::SentKey {
                    key: "y".to_owned(),
                    bytes: "79".to_owned(),
                }],
                settled: false,
                frame: None,
                inputs: crate::report::tui::InputCounts {
                    sent: 1,
                    consumed: 1,
                },
                screen: screen(),
                events: page(),
                exit: Some(exit()),
            }),
        );
        let reported = with_cleanup_problem(quit, problem);
        assert!(!reported.ok);
        assert_eq!(reported.command, Some(TuiCommand::Keys));
        assert_eq!(reported.session, named());
        let error = reported.error.expect("a failure");
        assert!(error.message.contains(problem), "{}", error.message);
        assert_eq!(error.exit, Some(exit()));
        assert_eq!(error.sent.expect("the keys").len(), 1);
        assert!(error.screen.is_some());

        // A failure that ended the session says it in its message.
        let mut ended = TuiError::new(TuiErrorKind::ProgramExited, "it ended");
        ended.exit = Some(exit());
        let failure = HarnessTui::failure(Some(TuiCommand::Delete), named(), ended);
        let error = with_cleanup_problem(failure, problem)
            .error
            .expect("a failure");
        assert!(error.message.starts_with("it ended; "), "{}", error.message);
        assert!(error.message.contains(problem));
        assert_eq!(error.exit, Some(exit()));

        // `close` has its cleanup.
        let close = HarnessTui::success(
            named(),
            TuiResult::Close(CloseResult {
                exit: exit(),
                screen: screen(),
                events: page(),
                recording: None,
                fixture: FixtureChanges {
                    removed: 0,
                    unexpected: Vec::new(),
                },
                residue: Vec::new(),
                cleanup: Cleanup {
                    removed: true,
                    problems: Vec::new(),
                },
            }),
        );
        let reported = with_cleanup_problem(close, problem);
        assert!(reported.ok);
        let Some(TuiResult::Close(close)) = reported.result else {
            panic!("a close result");
        };
        assert!(!close.cleanup.removed);
        assert_eq!(close.cleanup.problems, [problem]);
    }

    #[test]
    fn nothing_is_started_or_sent_for_a_command_that_is_not_valid() {
        let invalid = [
            Command::Keys {
                session: "0123abcd".to_owned(),
                keys: vec!["Enter".to_owned()],
                timeout: DEFAULT_KEYS_TIMEOUT,
            },
            Command::Keys {
                session: "0123abcd".to_owned(),
                keys: vec!["down".to_owned()],
                timeout: Duration::ZERO,
            },
            Command::Delete {
                session: "0123abcd".to_owned(),
                name: "../outside".to_owned(),
                kind: crate::scenario::EntryKind::File,
                timeout: DEFAULT_KEYS_TIMEOUT,
            },
            Command::Delete {
                session: "0123abcd".to_owned(),
                name: "*.bin".to_owned(),
                kind: crate::scenario::EntryKind::File,
                timeout: DEFAULT_KEYS_TIMEOUT,
            },
            Command::Open(OpenRequest {
                fixture: "delete-file".to_owned(),
                profile: crate::scenario::Profile::Default,
                size: (10, 4),
                record: false,
                idle_timeout: DEFAULT_IDLE_TIMEOUT,
                binary: "/bin/true".into(),
            }),
            Command::Open(OpenRequest {
                fixture: "delete-file".to_owned(),
                profile: crate::scenario::Profile::Default,
                size: (120, 40),
                record: false,
                idle_timeout: Duration::from_millis(5),
                binary: "/bin/true".into(),
            }),
        ];
        let state = tempfile::tempdir().expect("a state directory");
        let options = Options {
            state_dir: state.path().join("state"),
            launcher: super::super::command::Launcher {
                program: "/bin/false".into(),
                args: Vec::new(),
            },
        };

        for command in &invalid {
            let outcome = execute(&options, command);

            assert!(!outcome.is_success(), "{command:?}");
            assert_eq!(outcome.exit_code(), 2, "{command:?}");
            assert_eq!(
                outcome.document.error.as_ref().map(|error| error.kind),
                Some(TuiErrorKind::Usage),
                "{command:?}"
            );
        }
        assert!(!options.state_dir.exists(), "nothing was made");
    }

    #[test]
    fn a_session_that_is_not_there_is_reported_with_the_way_out() {
        let state = tempfile::tempdir().expect("a state directory");
        let options = Options {
            state_dir: state.path().to_path_buf(),
            launcher: super::super::command::Launcher {
                program: "/bin/false".into(),
                args: Vec::new(),
            },
        };

        let outcome = execute(
            &options,
            &Command::Screen {
                session: "0123abcd".to_owned(),
            },
        );

        let error = outcome.document.error.expect("an error");
        assert_eq!(error.kind, TuiErrorKind::NoSuchSession);
        assert!(error.message.contains("list"), "{}", error.message);
        assert_eq!(outcome.document.session.as_deref(), Some("0123abcd"));

        let bad = execute(
            &options,
            &Command::Close {
                session: "../etc".to_owned(),
            },
        );
        assert_eq!(
            bad.document.error.expect("an error").kind,
            TuiErrorKind::Usage
        );
    }

    fn session() -> (tempfile::TempDir, SessionDir) {
        let state = tempfile::tempdir().expect("a state directory");
        let session = SessionDir::create(state.path()).expect("a session directory");
        (state, session)
    }

    /// A process that stands in for a supervisor that neither answers nor ends.
    fn silent_supervisor() -> Child {
        Process::new("/bin/sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("sleep starts")
    }

    #[test]
    fn a_command_that_gives_up_withdraws_its_request_unless_the_supervisor_took_it() {
        let (_state, dir) = session();
        let queued = dir.submit(&Request::Close).expect("a request");

        let error = await_reply(&dir, &queued, Duration::from_millis(30)).expect_err("no reply");

        assert_eq!(error.kind, TuiErrorKind::Timeout);
        assert!(error.message.contains("withdrawn"), "{}", error.message);
        assert!(
            dir.take_request().expect("a read").is_none(),
            "the command reported a timeout, so its request must never run"
        );

        let taken = dir.submit(&Request::Screen).expect("a request");
        dir.take_request()
            .expect("a read")
            .expect("the supervisor takes it");
        let error = await_reply(&dir, &taken, Duration::from_millis(30)).expect_err("no reply");
        assert_eq!(error.kind, TuiErrorKind::Timeout);
        assert!(
            error.message.contains("may still be serving"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_reply_that_comes_as_the_command_gives_up_is_still_the_answer() {
        let (_state, dir) = session();
        let name = dir.submit(&Request::Screen).expect("a request");
        dir.take_request()
            .expect("a read")
            .expect("the supervisor takes it");
        let document = HarnessTui::failure(
            Some(TuiCommand::Screen),
            Some(dir.id().to_string()),
            TuiError::new(TuiErrorKind::Failed, "late"),
        );
        dir.reply(&name, &document).expect("a reply");

        assert_eq!(
            timed_out(&dir, &name, Duration::from_secs(1)).expect("the reply"),
            document
        );
    }

    #[test]
    fn a_supervisor_that_does_not_end_is_killed_and_its_session_removed() {
        let (_state, dir) = session();
        let mut child = silent_supervisor();
        let started = Instant::now();

        end_failed_start(&mut child, &dir, Duration::from_millis(100));

        assert!(
            started.elapsed() < Duration::from_secs(10),
            "every wait is bounded"
        );
        assert!(
            child.try_wait().expect("a status").is_some(),
            "the supervisor was killed and reaped"
        );
        assert!(
            !dir.exists(),
            "a session whose supervisor is known to be gone is cleaned however young it is"
        );
    }

    #[test]
    fn a_child_that_ends_is_reaped_and_one_that_does_not_is_given_up_on() {
        let mut quick = Process::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("sh starts");
        assert!(reaped(&mut quick, Duration::from_secs(10)));

        let mut silent = silent_supervisor();
        let started = Instant::now();
        assert!(!reaped(&mut silent, Duration::from_millis(50)));
        assert!(started.elapsed() < Duration::from_secs(5));
        silent.kill().expect("the stand-in is killed");
        silent.wait().expect("and reaped");
    }

    #[test]
    fn open_gives_up_on_a_supervisor_that_never_answers() {
        let (_state, dir) = session();
        let mut child = silent_supervisor();

        let error =
            await_open(&dir, &mut child, Duration::from_millis(100)).expect_err("no answer");

        assert_eq!(error.kind, TuiErrorKind::Timeout);
        assert!(error.message.contains("stopped and removed"));
        child.kill().expect("the stand-in is killed");
        child.wait().expect("and reaped");
    }
}
