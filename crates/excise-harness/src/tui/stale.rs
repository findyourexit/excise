//! Which sessions are alive, and what becomes of one whose supervisor is gone.
//!
//! A supervisor holds an exclusive lock on its session's `supervisor.lock` for as long as it
//! lives, so whether it is alive is a question the kernel answers, with no process id to reuse and
//! no clock to trust: a lock nobody holds belongs to a supervisor that ended. A session whose
//! supervisor ended without removing it (it was killed, or the machine went down) is stale. `open`
//! and `list` find stale sessions and clean them.
//!
//! Cleaning removes and kills things, so it acts only on what the driver can prove is its own:
//!
//! * a directory is a session only if its configuration is there and names it; anything else with
//!   a session's name is left, but for the skeleton a killed `open` makes (the request and reply
//!   directories, empty, and the temporary file of the configuration);
//! * a process is signaled only if the system shows the mark that only the session's processes
//!   carry in its environment, and nothing recorded of it (start time, executable) contradicts
//!   that ([`super::identity`]); it is looked at again immediately before the signal, and a
//!   process id that names something else now, or that cannot be told, is left alone and
//!   reported, and so is the session, so that the next `list` says it again;
//! * a workspace is removed only if it holds the marker of the session and the session's nonce,
//!   which is the last thing in it to go.

use std::{
    fs::{self, File, TryLockError},
    io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant, SystemTime},
};

use super::{
    identity::{self, ProcessIdentity, Standing},
    layout::{self, Config, SessionDir, State},
};
use crate::{
    report::tui::{SessionInfo, SessionState, StaleSession},
    safety::{kill_process, kill_process_group, process_group_exists, process_group_of},
};

/// How long a session directory without a supervisor's state counts as starting, not stale: the
/// time between `open` creating the directory and its supervisor taking the lock. A process that
/// starts at once takes a few milliseconds; a minute is for a machine so loaded that it does not.
const STARTING_GRACE: Duration = Duration::from_mins(1);
/// How long to wait before believing that a supervisor is gone: one that is ending removes its
/// lock a moment before it removes its directory.
const RECHECK_PAUSE: Duration = Duration::from_millis(25);
/// How long to wait for a killed program's process group to be gone.
const KILL_WAIT: Duration = Duration::from_secs(2);

/// Whether a session's supervisor is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Liveness {
    /// Something holds the supervisor's lock.
    Alive,
    /// Nothing does, or there is no lock to hold.
    Gone,
}

/// Asks the kernel whether the supervisor of `session` is alive.
pub(super) fn liveness(session: &SessionDir) -> Liveness {
    let file = match File::open(session.lock()) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Liveness::Gone,
        // A lock that cannot be looked at is not a reason to remove a session.
        Err(_) => return Liveness::Alive,
    };
    match file.try_lock_shared() {
        Ok(()) => {
            let _ = file.unlock();
            Liveness::Gone
        }
        Err(TryLockError::WouldBlock | TryLockError::Error(_)) => Liveness::Alive,
    }
}

/// Finds the stale sessions in `state_dir` and cleans them.
///
/// # Errors
///
/// Returns the error of listing the directory, which includes a state directory that is a link:
/// the sessions behind a link are not the driver's.
pub(super) fn sweep(state_dir: &Path) -> io::Result<Vec<StaleSession>> {
    let mut stale = Vec::new();
    for session in layout::sessions(state_dir)? {
        if let Some(report) = sweep_one(&session) {
            stale.push(report);
        }
    }
    Ok(stale)
}

/// Whether the supervisor of `session` is gone: nothing holds its lock, and the session is not
/// one that is still starting.
///
/// A session that has never had a supervisor state is given a while, since its supervisor takes
/// the lock a moment after the session directory is made. And a supervisor that is ending removes
/// its lock a moment before it removes its directory, so a session that is not there any longer,
/// or is alive again after a short pause, is not gone either.
pub(super) fn is_gone(session: &SessionDir) -> bool {
    if liveness(session) == Liveness::Alive {
        return false;
    }
    if session.read_state().is_none() && age(session) < STARTING_GRACE {
        return false;
    }
    thread::sleep(RECHECK_PAUSE);
    session.exists() && liveness(session) == Liveness::Gone
}

/// Cleans `session` if its supervisor is gone, and says so.
pub(super) fn sweep_one(session: &SessionDir) -> Option<StaleSession> {
    is_gone(session).then(|| clean(session))
}

/// How long ago the session directory last changed.
fn age(session: &SessionDir) -> Duration {
    fs::metadata(session.path())
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .unwrap_or(Duration::MAX)
}

/// Cleans a session whose supervisor is gone, or is known to be: what the session started is
/// stopped if it can be proven to be the session's, its workspace and its directory are removed,
/// and whatever cannot be proven is left and reported.
pub(super) fn clean(session: &SessionDir) -> StaleSession {
    clean_by(session, &identity::standing)
}

/// How a process id is judged: whether it is the process of a session, as [`identity::standing`]
/// says, except in the tests of what cleaning does with each answer.
type Judge<'a> = &'a dyn Fn(u32, Option<&ProcessIdentity>, &str) -> Standing;

fn clean_by(session: &SessionDir, judge: Judge<'_>) -> StaleSession {
    let Some(config) = session
        .read_config()
        .ok()
        .filter(|config| config.session == session.id().as_str())
    else {
        return leftover(session);
    };
    let state = session.read_state();
    let mut problems = Vec::new();

    if let Err(problem) = stop_program(&config, state.as_ref(), judge) {
        problems.push(problem);
    }
    // Whatever the program may still be using stays while its fate is unknown.
    if problems.is_empty()
        && let Err(problem) =
            layout::remove_workspace(&config.work_base, session.id(), &config.nonce)
    {
        problems.push(problem);
    }
    if problems.is_empty()
        && let Err(error) = session.remove()
    {
        problems.push(format!(
            "cannot remove `{}`: {error}",
            session.path().display()
        ));
    }

    StaleSession {
        session: session.id().to_string(),
        reason: if state.is_some() {
            "its supervisor is gone: nothing holds the session's lock".to_owned()
        } else {
            "its supervisor never finished starting: nothing holds the session's lock".to_owned()
        },
        fixture: Some(config.fixture.clone()),
        recording: config
            .record
            .as_ref()
            .filter(|path| path.exists())
            .map(|path| path.display().to_string()),
        cleaned: problems.is_empty(),
        problems,
    }
}

/// A directory named like a session whose configuration is missing, cannot be read, or names
/// another session: nothing proves that this driver made it, so it is left alone and reported.
/// The one thing that goes is the skeleton that an `open` killed while it made the directory
/// leaves: the request and reply directories, empty, both or either, and the temporary file the
/// configuration is written through, and nothing else. Removing those can lose nothing.
fn leftover(session: &SessionDir) -> StaleSession {
    let skeleton = skeleton_entries(session).is_some_and(|entries| {
        entries.iter().all(|(path, directory)| {
            removed(if *directory {
                fs::remove_dir(path)
            } else {
                fs::remove_file(path)
            })
        }) && removed(fs::remove_dir(session.path()))
    });
    StaleSession {
        session: session.id().to_string(),
        reason: "its configuration is missing or is not this session's".to_owned(),
        fixture: None,
        recording: None,
        cleaned: skeleton,
        problems: if skeleton {
            Vec::new()
        } else {
            vec![format!(
                "`{}` is named like a session, but nothing proves that this driver made it (its \
                 configuration is missing, cannot be read, or names another session), so it was \
                 left alone",
                session.path().display()
            )]
        },
    }
}

/// Whether a removal left the path gone: it was removed, or it was not there.
fn removed(result: io::Result<()>) -> bool {
    match result {
        Ok(()) => true,
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

/// What the session directory holds, if it holds only what a killed `open` leaves: the request
/// and reply directories, empty and not links, and the temporary files of the configuration's
/// write. Each is returned with whether it is a directory. `None` if there is anything else, or
/// anything that cannot be looked at.
fn skeleton_entries(session: &SessionDir) -> Option<Vec<(PathBuf, bool)>> {
    let mut found = Vec::new();
    for entry in fs::read_dir(session.path()).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).ok()?;
        let name = entry.file_name();
        let name = name.to_str()?;
        if matches!(name, "req" | "rep") {
            if !metadata.is_dir() || fs::read_dir(&path).ok()?.next().is_some() {
                return None;
            }
            found.push((path, true));
        } else if metadata.is_file() && layout::is_config_temporary(name) {
            found.push((path, false));
        } else {
            return None;
        }
    }
    Some(found)
}

/// Stops what the session started that is still running, if it can be proven to be the
/// session's, and leaves everything else.
///
/// The program the supervisor recorded is stopped if the system shows the mark in its
/// environment and nothing it recorded of the program contradicts that. A program that was
/// started but never recorded, because the supervisor ended in between, is found by its mark
/// among the processes that have the run copy in their arguments. A process id that now belongs
/// to another process is left alone, and the session has nothing running; a process that cannot
/// be told is left alone, and the session is left with it.
///
/// # Errors
///
/// Returns why a process of the session could not be dealt with, or why it could not be told.
fn stop_program(config: &Config, state: Option<&State>, judge: Judge<'_>) -> Result<(), String> {
    let nonce = config.nonce.as_str();
    let recorded = state.and_then(|state| state.child.as_ref());
    let mut programs: Vec<u32> = Vec::new();

    if let Some(state) = state
        && let Some(pid) = state.child_pid
    {
        match judge(pid, recorded, nonce) {
            Standing::Ours => programs.push(pid),
            Standing::Gone | Standing::Another(_) => {}
            Standing::Unknown(why) => {
                return Err(format!(
                    "cannot tell whether process {pid} is the session's program ({why}), so it \
                     was not signaled and the session was left"
                ));
            }
        }
    }
    if let Some(root) = state.and_then(|state| state.root.as_deref()) {
        for pid in identity::carrying(root, nonce) {
            if !programs.contains(&pid) {
                programs.push(pid);
            }
        }
    }
    for pid in programs {
        let recorded = recorded.filter(|identity| identity.pid == pid);
        kill_program(pid, &|| judge(pid, recorded, nonce))?;
    }
    Ok(())
}

/// Kills the process `pid`, which was proven to be a process of the session, and its whole
/// process group when it leads one (the pseudo-terminal library starts the program with
/// `setsid`), and waits a bounded time for it to be gone.
///
/// `recheck` says what `pid` is now, and it is asked after the process group is known and
/// immediately before the signal: a process id is only a name, so a process that ended since it
/// was proven can have given its id to another one, and nothing is signaled unless the process
/// is still the session's. What is left between that look and the signal is the time the system
/// takes to deliver one, and a process id is not given again until every other id has been given.
fn kill_program(pid: u32, recheck: &dyn Fn() -> Standing) -> Result<(), String> {
    let Some(group) = process_group_of(pid) else {
        return Ok(());
    };
    match recheck() {
        Standing::Ours => {}
        Standing::Gone | Standing::Another(_) => return Ok(()),
        Standing::Unknown(why) => {
            return Err(format!(
                "cannot tell whether process {pid} is still the session's program ({why}), so it \
                 was not signaled and the session was left"
            ));
        }
    }
    let alone = group != pid;
    let signaled = if alone {
        kill_process(pid)
    } else {
        kill_process_group(pid)
    };
    signaled
        .map_err(|error| format!("cannot kill the session's program (process {pid}): {error}"))?;
    let gone = || {
        if alone {
            !identity::exists(pid)
        } else {
            !process_group_exists(pid)
        }
    };
    let deadline = Instant::now() + KILL_WAIT;
    while !gone() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    if gone() {
        Ok(())
    } else {
        Err(format!(
            "the session's program (process {pid}) is still there after it was killed"
        ))
    }
}

/// The sessions in `state_dir` whose supervisors are alive.
///
/// # Errors
///
/// Returns the error of listing the directory.
pub(super) fn running(state_dir: &Path) -> io::Result<Vec<SessionInfo>> {
    let mut found = Vec::new();
    for session in layout::sessions(state_dir)? {
        if liveness(&session) != Liveness::Alive {
            continue;
        }
        let Ok(config) = session.read_config() else {
            continue;
        };
        found.push(info(&session, &config));
    }
    found.sort_by(|left, right| {
        (&left.started_at, &left.session).cmp(&(&right.started_at, &right.session))
    });
    Ok(found)
}

fn info(session: &SessionDir, config: &Config) -> SessionInfo {
    let state = session.read_state();
    SessionInfo {
        session: session.id().to_string(),
        state: if state.as_ref().is_some_and(|state| state.ready) {
            SessionState::Running
        } else {
            SessionState::Starting
        },
        fixture: config.fixture.clone(),
        profile: config.profile,
        size: config.terminal_size(),
        supervisor_pid: state.map(|state| u64::from(state.supervisor_pid)),
        started_at: config.started_at.clone(),
        idle_timeout_ms: config.idle_timeout_ms,
        recording: config
            .record
            .as_ref()
            .map(|path| path.display().to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs::OpenOptions,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{
        scenario::Profile,
        tui::{identity::ProcessIdentity, testing::Program},
    };

    /// A run copy path and a nonce that no other test uses, because the tests run in parallel and
    /// find processes by exactly these.
    fn unique(label: &str) -> (PathBuf, String) {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let number = COUNTER.fetch_add(1, Ordering::Relaxed);
        let process = std::process::id();
        (
            PathBuf::from(format!("/tmp/xh-tui-{label}-{process}-{number}/run-copy")),
            format!("{process:08x}{number:024x}"),
        )
    }

    fn config(session: &SessionDir, work_base: &Path, nonce: &str) -> Config {
        Config {
            session: session.id().to_string(),
            fixture: "delete-file".to_owned(),
            profile: Profile::Default,
            cols: 120,
            rows: 40,
            record: None,
            idle_timeout_ms: 900_000,
            binary: PathBuf::from("/bin/excise"),
            work_base: work_base.to_path_buf(),
            started_at: "2026-10-04T00:00:00.000Z".to_owned(),
            nonce: nonce.to_owned(),
        }
    }

    /// A session directory whose supervisor "lives" for as long as the returned file does.
    fn live_supervisor(session: &SessionDir) -> File {
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(session.lock())
            .expect("the lock file");
        lock.try_lock().expect("the lock");
        lock
    }

    /// [`stop_program`] as cleaning does it: judging a process id by what the system shows of it.
    fn stop(config: &Config, state: Option<&State>) -> Result<(), String> {
        stop_program(config, state, &identity::standing)
    }

    /// Waits until `condition` holds, for at most ten seconds.
    fn until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "gave up waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Makes a directory look as if it had been there for an hour.
    fn age_an_hour(dir: &Path) {
        let old = SystemTime::now() - Duration::from_hours(1);
        File::open(dir)
            .and_then(|file| file.set_modified(old))
            .expect("an old directory");
    }

    #[test]
    fn a_held_lock_is_a_live_supervisor_and_a_released_one_is_not() {
        let state = tempfile::tempdir().expect("a state directory");
        let session = SessionDir::create(state.path()).expect("a session");
        assert_eq!(liveness(&session), Liveness::Gone, "no lock file yet");

        let lock = live_supervisor(&session);
        assert_eq!(liveness(&session), Liveness::Alive);
        assert_eq!(
            liveness(&session),
            Liveness::Alive,
            "asking changes nothing"
        );

        drop(lock);
        // A process that another thread is forking at this instant holds the file for a moment,
        // so a lock that was released can take a moment to read as released.
        until("the lock to read as released", || {
            liveness(&session) == Liveness::Gone
        });
    }

    #[test]
    fn a_live_session_is_never_swept_and_is_listed() {
        let state = tempfile::tempdir().expect("a state directory");
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(state.path()).expect("a session");
        session
            .write_config(&config(&session, work.path(), "a-nonce"))
            .expect("config");
        session
            .write_state(&State {
                supervisor_pid: 11,
                ready: true,
                ..State::default()
            })
            .expect("state");
        let _alive = live_supervisor(&session);

        assert!(sweep(state.path()).expect("a sweep").is_empty());
        let listed = running(state.path()).expect("sessions");

        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session, session.id().as_str());
        assert_eq!(listed[0].state, SessionState::Running);
        assert_eq!(listed[0].supervisor_pid, Some(11));
        assert!(session.exists());
    }

    #[test]
    fn a_session_that_is_still_starting_is_left_alone() {
        let state = tempfile::tempdir().expect("a state directory");
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(state.path()).expect("a session");
        session
            .write_config(&config(&session, work.path(), "a-nonce"))
            .expect("config");

        // No lock and no state yet, but the directory was made a moment ago.
        assert!(sweep(state.path()).expect("a sweep").is_empty());
        assert!(session.exists());
        assert!(running(state.path()).expect("sessions").is_empty());
    }

    #[test]
    fn a_session_whose_supervisor_is_gone_is_reported_and_cleaned_with_its_workspace() {
        let state = tempfile::tempdir().expect("a state directory");
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(state.path()).expect("a session");
        let mut config = config(&session, work.path(), "a-nonce");
        config.record = Some(layout::recording_path(state.path(), session.id()));
        fs::write(config.record.as_ref().expect("a recording"), b"{}\n").expect("a recording");
        session.write_config(&config).expect("config");
        session
            .write_state(&State {
                supervisor_pid: 1,
                ready: true,
                ..State::default()
            })
            .expect("state");
        let workspace = layout::Workspace::create(work.path(), session.id(), "a-nonce")
            .expect("a workspace")
            .path();
        fs::create_dir_all(workspace.join("run/deep")).expect("a run copy");
        drop(live_supervisor(&session));

        let found = sweep(state.path()).expect("a sweep");

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].session, session.id().as_str());
        assert!(found[0].cleaned, "{:?}", found[0].problems);
        assert_eq!(found[0].fixture.as_deref(), Some("delete-file"));
        assert!(found[0].reason.contains("supervisor is gone"));
        assert!(!session.exists());
        assert!(!workspace.exists());
        let recording = config.record.expect("a recording");
        assert_eq!(
            found[0].recording.as_deref(),
            Some(recording.to_string_lossy().as_ref())
        );
        assert!(recording.exists(), "a recording is kept");
    }

    #[test]
    fn a_workspace_that_is_not_the_sessions_is_left_and_the_session_is_kept_to_say_so() {
        let state = tempfile::tempdir().expect("a state directory");
        let work = tempfile::tempdir().expect("a work base");
        // A directory with the session's name that nobody marked, and the workspace of another
        // checkout's session that drew the same id (ids are 32 random bits, unique only within
        // one state directory), are both somebody else's.
        let unmarked = SessionDir::create(state.path()).expect("a session");
        let marked_by_another = SessionDir::create(state.path()).expect("a session");
        for session in [&unmarked, &marked_by_another] {
            session
                .write_config(&config(session, work.path(), "a-nonce"))
                .expect("config");
            session
                .write_state(&State {
                    supervisor_pid: 1,
                    ..State::default()
                })
                .expect("state");
        }
        let impostor = layout::workspace_path(work.path(), unmarked.id());
        fs::create_dir(&impostor).expect("a directory that is not the workspace");
        fs::write(impostor.join("precious"), b"keep").expect("a file");
        let another = layout::Workspace::create(work.path(), marked_by_another.id(), "another")
            .expect("another session's workspace");
        fs::write(another.path().join("precious"), b"keep").expect("a file");

        let found = sweep(state.path()).expect("a sweep");

        assert_eq!(found.len(), 2);
        for report in &found {
            assert!(!report.cleaned);
            assert_eq!(report.problems.len(), 1, "{:?}", report.problems);
        }
        let problem_of = |session: &SessionDir| {
            found
                .iter()
                .find(|report| report.session == session.id().as_str())
                .map(|report| report.problems[0].clone())
                .expect("a report")
        };
        assert!(problem_of(&unmarked).contains("does not carry"));
        assert!(problem_of(&marked_by_another).contains("another session's marker"));
        assert!(impostor.join("precious").exists());
        assert!(another.path().join("precious").exists());
        assert!(
            unmarked.exists() && marked_by_another.exists(),
            "the sessions stay so that the leftovers are reported again"
        );
    }

    #[test]
    fn a_directory_that_is_not_provably_a_session_is_left_alone() {
        let state = tempfile::tempdir().expect("a state directory");
        let work = tempfile::tempdir().expect("a work base");
        // Named like a session, with something in it that the driver did not make...
        let stranger = SessionDir::create(state.path()).expect("a directory");
        fs::write(stranger.path().join("precious"), b"keep").expect("a file");
        // ...one whose configuration names another session...
        let copied = SessionDir::create(state.path()).expect("a directory");
        let someone_else = SessionDir::at(
            state.path(),
            layout::SessionId::parse("feedface").expect("an id"),
        );
        copied
            .write_config(&config(&someone_else, work.path(), "a-nonce"))
            .expect("a configuration that is another session's");
        // ...one whose configuration has no mark to compare anything with...
        let unmarked = SessionDir::create(state.path()).expect("a directory");
        let mut document =
            serde_json::to_value(config(&unmarked, work.path(), "a-nonce")).expect("JSON");
        document.as_object_mut().expect("an object").remove("nonce");
        fs::write(
            unmarked.path().join("config.json"),
            serde_json::to_vec(&document).expect("JSON"),
        )
        .expect("a configuration without a mark");
        // ...and the skeleton that a killed `open` leaves, which holds nothing.
        let skeleton = SessionDir::create(state.path()).expect("a directory");
        for dir in [&stranger, &copied, &unmarked, &skeleton] {
            age_an_hour(dir.path());
        }

        let found = sweep(state.path()).expect("a sweep");

        let by_session = |session: &SessionDir| {
            found
                .iter()
                .find(|report| report.session == session.id().as_str())
                .expect("a report")
        };
        assert!(stranger.path().join("precious").exists());
        assert!(
            stranger.path().join("req").is_dir() && stranger.path().join("rep").is_dir(),
            "nothing in it was touched"
        );
        assert!(copied.exists() && unmarked.exists());
        for left in [&stranger, &copied, &unmarked] {
            assert!(!by_session(left).cleaned);
            assert!(
                by_session(left).problems[0].contains("nothing proves"),
                "{:?}",
                by_session(left).problems
            );
        }
        assert!(by_session(&skeleton).cleaned);
        assert!(
            !skeleton.exists(),
            "an empty skeleton holds nothing to lose"
        );
    }

    #[test]
    fn a_state_directory_that_is_a_link_is_never_swept() {
        let base = tempfile::tempdir().expect("a base");
        let target = base.path().join("somewhere-else");
        fs::create_dir(&target).expect("a directory");
        let child = SessionDir::create(&target).expect("a directory named like a session");
        fs::write(child.path().join("precious"), b"keep").expect("a file");
        age_an_hour(child.path());
        let link = base.path().join("excise-tui");
        std::os::unix::fs::symlink(&target, &link).expect("a link");

        let swept = sweep(&link).expect_err("a link is refused");
        let listed = running(&link).expect_err("and is not listed");

        assert!(swept.to_string().contains("link"), "{swept}");
        assert!(listed.to_string().contains("link"), "{listed}");
        assert!(child.path().join("precious").exists());
        assert!(
            sweep(&base.path().join("not-there"))
                .expect("a missing directory has no sessions")
                .is_empty()
        );
    }

    #[test]
    fn the_program_the_session_recorded_is_killed_with_its_group() {
        let (root, nonce) = unique("recorded");
        let program = Program::start(&root, Some(&nonce));
        let pid = program.pid();
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(work.path()).expect("a session");
        let state = State {
            supervisor_pid: 1,
            ready: true,
            root: Some(root),
            child_pid: Some(pid),
            child: ProcessIdentity::once_marked(pid, &nonce, Duration::from_secs(10)),
        };
        assert!(process_group_exists(pid));

        assert_eq!(
            stop(&config(&session, work.path(), &nonce), Some(&state)),
            Ok(())
        );

        assert!(!process_group_exists(pid));
        assert!(!program.running());
    }

    #[test]
    fn a_process_that_took_over_the_recorded_id_is_never_killed() {
        // The id the session recorded now belongs to another process, which has the run copy among
        // its arguments, as a tool somebody ran against the path `open` printed would.
        let (root, nonce) = unique("taken-over");
        let stranger = Program::start(&root, None);
        let pid = stranger.pid();
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(work.path()).expect("a session");
        // What the session recorded of its program: it started long before the process that has
        // the id now, which is the one the system shows.
        let recorded = ProcessIdentity {
            pid,
            started: 1,
            exe: None,
        };
        let state = State {
            supervisor_pid: 1,
            ready: true,
            root: Some(root),
            child_pid: Some(pid),
            child: Some(recorded),
        };

        assert_eq!(
            stop(&config(&session, work.path(), &nonce), Some(&state)),
            Ok(())
        );

        assert!(stranger.running(), "it is not the session's program");
        assert!(process_group_exists(pid));
    }

    #[test]
    fn a_process_that_cannot_be_told_is_left_and_the_session_with_it() {
        // The system shows nothing of the process's environment, and the session recorded no
        // identity for it: nothing says whose it is.
        let (root, nonce) = unique("untold");
        let stranger = Program::start(&root, None);
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(work.path()).expect("a session");
        session
            .write_config(&config(&session, work.path(), &nonce))
            .expect("config");
        session
            .write_state(&State {
                supervisor_pid: 1,
                ready: true,
                root: Some(root),
                child_pid: Some(stranger.pid()),
                child: None,
            })
            .expect("state");
        let cannot_tell = |_: u32, _: Option<&ProcessIdentity>, _: &str| {
            Standing::Unknown("the system shows nothing of it".to_owned())
        };

        let report = clean_by(&session, &cannot_tell);

        assert!(!report.cleaned);
        assert!(
            report.problems[0].contains("cannot tell"),
            "{:?}",
            report.problems
        );
        assert!(
            stranger.running(),
            "an unidentified process is never signaled"
        );
        assert!(
            session.exists(),
            "the session stays so that it is reported again"
        );
    }

    #[test]
    fn a_program_that_was_never_recorded_is_found_by_its_mark_and_nothing_else_is() {
        let (root, nonce) = unique("lost");
        let lost = Program::start(&root, Some(&nonce));
        let a_tool_run_against_the_root = Program::start(&root, None);
        let another_sessions_program = Program::start(&root, Some("another-nonce"));
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(work.path()).expect("a session");
        // The supervisor ended after it recorded the run copy and before it recorded the program.
        let state = State {
            supervisor_pid: 1,
            ready: false,
            root: Some(root.clone()),
            child_pid: None,
            child: None,
        };
        until("the program to be listed by the system", || {
            identity::carrying(&root, &nonce).contains(&lost.pid())
        });

        assert_eq!(
            stop(&config(&session, work.path(), &nonce), Some(&state)),
            Ok(())
        );

        assert!(
            !lost.running(),
            "the program carrying the mark was found and killed"
        );
        assert!(a_tool_run_against_the_root.running());
        assert!(another_sessions_program.running());
    }

    #[test]
    fn a_process_that_does_not_lead_a_group_is_killed_alone() {
        // A group can hold processes that nothing proves are the session's, so only a process
        // that leads its group takes the group with it.
        let (root, nonce) = unique("alone");
        let leader = Program::start(&root, None);
        let member = Program::start_in_group_of(&leader, &root, &nonce);
        until("the member to be in the leader's group", || {
            process_group_of(member.pid()) == Some(leader.pid())
        });

        until("the member to show the mark", || {
            identity::standing(member.pid(), None, &nonce) == Standing::Ours
        });

        assert_eq!(
            kill_program(member.pid(), &|| identity::standing(
                member.pid(),
                None,
                &nonce
            )),
            Ok(())
        );

        assert!(!member.running());
        assert!(leader.running(), "its group was not killed with it");
    }

    #[test]
    fn nothing_is_signaled_for_a_session_that_ran_nothing() {
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(work.path()).expect("a session");
        let (_, nonce) = unique("nothing");
        let config = config(&session, work.path(), &nonce);

        assert_eq!(stop(&config, None), Ok(()));
        assert_eq!(
            stop(
                &config,
                Some(&State {
                    supervisor_pid: 1,
                    ..State::default()
                })
            ),
            Ok(())
        );
    }

    #[test]
    fn what_is_asked_again_at_the_moment_of_the_signal_decides_it() {
        // The process was proven to be the session's some time ago; `kill_program` looks again
        // after it knows the group and just before the signal, and that answer is the one that
        // counts.
        let (root, _) = unique("recheck");
        let stranger = Program::start(&root, None);
        let pid = stranger.pid();

        let another = || Standing::Another("it is another process".to_owned());
        assert_eq!(kill_program(pid, &another), Ok(()));
        assert_eq!(kill_program(pid, &|| Standing::Gone), Ok(()));
        let refused = kill_program(pid, &|| {
            Standing::Unknown("the system shows nothing of it".to_owned())
        })
        .expect_err("a process that cannot be told");
        assert!(refused.contains("cannot tell"), "{refused}");
        assert!(stranger.running(), "nothing was signaled");
        assert!(process_group_exists(pid));

        assert_eq!(kill_program(pid, &|| Standing::Ours), Ok(()));
        assert!(!stranger.running());
    }

    #[test]
    fn a_process_that_took_the_id_between_the_proof_and_the_signal_is_left() {
        let (root, nonce) = unique("changed-hands");
        let stranger = Program::start(&root, None);
        let work = tempfile::tempdir().expect("a work base");
        let session = SessionDir::create(work.path()).expect("a session");
        let state = State {
            supervisor_pid: 1,
            ready: true,
            root: Some(root),
            child_pid: Some(stranger.pid()),
            child: None,
        };
        // The first look proves it, as it would have the session's program; by the second, the
        // program has ended and another process has its id.
        let asked = std::cell::Cell::new(0);
        let judge = |_: u32, _: Option<&ProcessIdentity>, _: &str| {
            asked.set(asked.get() + 1);
            if asked.get() == 1 {
                Standing::Ours
            } else {
                Standing::Another("it took the id".to_owned())
            }
        };

        assert_eq!(
            stop_program(&config(&session, work.path(), &nonce), Some(&state), &judge),
            Ok(())
        );

        assert_eq!(asked.get(), 2, "it was asked again before the signal");
        assert!(stranger.running(), "the process that took the id was left");
    }

    #[test]
    fn a_killed_open_is_cleaned_whatever_part_of_the_directory_it_made() {
        let state = tempfile::tempdir().expect("a state directory");
        let session = || SessionDir::create(state.path()).expect("a session");
        // A kill can come before either directory was made, between them, or after both, and
        // before the configuration was renamed into place.
        let only_requests = session();
        fs::remove_dir(only_requests.replies()).expect("one directory");
        let only_replies = session();
        fs::remove_dir(only_replies.requests()).expect("one directory");
        let neither = session();
        fs::remove_dir(neither.requests()).expect("a directory");
        fs::remove_dir(neither.replies()).expect("a directory");
        let mid_write = session();
        fs::write(mid_write.path().join("config.json.tmp-4242-7"), b"{")
            .expect("a configuration that was being written");
        // Anything else in the directory is not what a killed `open` leaves.
        let with_a_note = session();
        fs::write(with_a_note.path().join("notes.txt"), b"keep").expect("a file");
        let busy = session();
        fs::write(busy.requests().join("a.json"), b"{}").expect("a request");
        let cleaned = [&only_requests, &only_replies, &neither, &mid_write];
        let kept = [&with_a_note, &busy];
        for dir in cleaned.iter().chain(kept.iter()) {
            age_an_hour(dir.path());
        }

        let found = sweep(state.path()).expect("a sweep");

        let report = |dir: &SessionDir| {
            found
                .iter()
                .find(|report| report.session == dir.id().as_str())
                .expect("a report")
        };
        for dir in cleaned {
            assert!(report(dir).cleaned, "{:?}", report(dir).problems);
            assert!(!dir.exists());
        }
        for dir in kept {
            assert!(!report(dir).cleaned);
            assert!(dir.exists());
        }
        assert!(with_a_note.path().join("notes.txt").exists());
        assert!(busy.requests().join("a.json").exists());
    }
}
