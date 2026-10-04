//! Which process is which.
//!
//! A process id names a process only for as long as that process lives. When it ends, the number
//! can go to another process, one that has nothing to do with the session, and that process can
//! have been started with the same arguments: `open` prints the path of the run copy, and anyone
//! may run a tool against it. So nothing here signals a process because of its id, or because of
//! its command line. A session keeps two kinds of evidence, each recorded before anything can lose
//! it:
//!
//! * **The mark.** `open` makes a random value, the session's *nonce*, and records it in the
//!   session's configuration before the supervisor exists. The supervisor puts it in the
//!   environment of the program ([`NONCE_VARIABLE`]) and in the marker of the session's workspace,
//!   and every process the program starts inherits it. No other process has it, whatever its id
//!   and its arguments.
//! * **The identity.** Once the program shows the mark, the supervisor records what the
//!   operating system says about it: when it started and which executable it runs
//!   ([`ProcessIdentity`]). A process id that now names a process with another start time or
//!   another executable is another process. The identity can only rule a process out: a start
//!   time in whole seconds and a path are shared by any process that takes the id in the same
//!   second and runs the same program, so they never make a process the session's.
//!
//! [`standing`] weighs both for one process id. A process is the session's only if the system
//! shows the mark in its environment now. One whose environment the system does not show is not
//! known, whatever was recorded of it: it is left alone and reported, never signaled.
//! [`carrying`] finds the processes that carry a session's mark, for the case where the
//! supervisor ended before it could record its program.
//!
//! # A process that is being started
//!
//! Between a process being created and its program being loaded, the system shows it as its
//! parent: the same command line, the same environment, and the same executable, or, a moment
//! later, no environment at all. A reading made then is not about the program, and none of this
//! module's answers rests on one. The parent's environment carries the mark only if the parent
//! is a process of the session, and then the child is too, because it descends from it; an
//! environment that is not shown is [`Standing::Unknown`]; and the identity is recorded only
//! from a reading that shows the mark ([`ProcessIdentity::once_marked`]), so that it describes
//! the program and not the process that started it.

use std::{
    ffi::OsString,
    fmt::Write as _,
    fs::File,
    io::{self, Read as _},
    path::Path,
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sysinfo::{Pid, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System, UpdateKind};

/// The environment variable that carries a session's nonce in the program.
pub(super) const NONCE_VARIABLE: &str = "XH_TUI_SESSION";

/// A random value only the processes of one session carry: 128 bits, as 32 lowercase hexadecimal
/// digits.
///
/// # Errors
///
/// Returns the error of reading the system's source of randomness.
pub(super) fn new_nonce() -> io::Result<String> {
    let mut bytes = [0_u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    }))
}

/// What the operating system said about a process when it was recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ProcessIdentity {
    /// The process id it was recorded under: only a name, which another process can take later.
    pub(super) pid: u32,
    /// When the process started, in seconds since the epoch, as the system reports it.
    pub(super) started: u64,
    /// The path of its executable, as the system reports it, when it can say.
    pub(super) exe: Option<String>,
}

impl ProcessIdentity {
    /// What the system says about process `pid` once it shows the mark `nonce` in its
    /// environment, which is when it is the program of the session and not a process that is
    /// still being started (see the module's note on those). `None` if the process is not there,
    /// or has not shown the mark within `bound`.
    pub(super) fn once_marked(pid: u32, nonce: &str, bound: Duration) -> Option<Self> {
        let deadline = Instant::now() + bound;
        loop {
            let seen = look(pid, true)?;
            if seen
                .environment
                .as_deref()
                .is_some_and(|environment| holds(environment, nonce))
            {
                return Some(Self {
                    pid,
                    started: seen.started,
                    exe: seen.exe,
                });
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Whether a process with this id is there and is not a zombie, which nobody can signal to any
/// purpose: the question a kill asks once it has been delivered.
pub(super) fn exists(pid: u32) -> bool {
    look(pid, false).is_some()
}

/// What the system shows about a process.
struct Seen {
    started: u64,
    exe: Option<String>,
    /// The environment, if it was asked for and the system showed it; an environment that comes
    /// back empty is one the system did not show, because no program of a session runs without
    /// one.
    environment: Option<Vec<OsString>>,
}

/// Looks at process `pid`. A zombie, which nobody can signal to any purpose, is not there.
fn look(pid: u32, with_environment: bool) -> Option<Seen> {
    let target = Pid::from_u32(pid);
    let mut kind = ProcessRefreshKind::nothing().with_exe(UpdateKind::Always);
    if with_environment {
        kind = kind.with_environ(UpdateKind::Always);
    }
    let mut system = System::new();
    if system.refresh_processes_specifics(ProcessesToUpdate::Some(&[target]), false, kind) == 0 {
        return None;
    }
    let process = system.process(target)?;
    if process.status() == ProcessStatus::Zombie {
        return None;
    }
    Some(Seen {
        started: process.start_time(),
        exe: process
            .exe()
            .map(|path| path.to_string_lossy().into_owned()),
        environment: (with_environment && !process.environ().is_empty())
            .then(|| process.environ().to_vec()),
    })
}

/// Whether `environment` holds the nonce `nonce`.
fn holds(environment: &[OsString], nonce: &str) -> bool {
    let wanted = format!("{NONCE_VARIABLE}={nonce}");
    environment
        .iter()
        .any(|entry| entry.to_str() == Some(wanted.as_str()))
}

/// Whether a process id is, now, the process of a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Standing {
    /// No process has this id, or it is a zombie.
    Gone,
    /// The process is the session's: nothing that was recorded contradicts it, and something
    /// confirms it.
    Ours,
    /// A process has this id and it is another one; why is said.
    Another(String),
    /// The system will not show enough to tell, and the session recorded nothing else to go by;
    /// why is said. Such a process is never signaled.
    Unknown(String),
}

/// Whether process `pid` is the process of the session whose mark is `nonce`, given the identity
/// the session recorded for it, if it recorded one.
///
/// The process is the session's only if nothing contradicts it and the mark confirms it: a
/// recorded identity that does not match (another start time, another executable, another
/// process id) or an environment without the mark makes it another process, and the mark in its
/// environment makes it the session's. An environment that is not shown settles nothing, and
/// neither does a recorded identity that matches: the process is [`Standing::Unknown`], and the
/// identity, which another process can have in common with the session's, is not a proof.
pub(super) fn standing(pid: u32, recorded: Option<&ProcessIdentity>, nonce: &str) -> Standing {
    look(pid, true).map_or(Standing::Gone, |seen| weigh(pid, &seen, recorded, nonce))
}

/// What the system showed of process `pid`, held against what the session recorded.
fn weigh(pid: u32, seen: &Seen, recorded: Option<&ProcessIdentity>, nonce: &str) -> Standing {
    if let Some(recorded) = recorded {
        if recorded.pid != pid {
            return Standing::Another(format!(
                "the identity the session recorded is that of process {}",
                recorded.pid
            ));
        }
        if seen.started != recorded.started {
            return Standing::Another(format!(
                "it started at {} (seconds since the epoch) and the session's process at {}",
                seen.started, recorded.started
            ));
        }
        if let (Some(expected), Some(actual)) = (&recorded.exe, &seen.exe)
            && expected != actual
        {
            return Standing::Another(format!(
                "it runs `{actual}` and the session's process ran `{expected}`"
            ));
        }
    }
    match &seen.environment {
        Some(environment) if holds(environment, nonce) => Standing::Ours,
        Some(_) => Standing::Another("it does not carry the session's mark".to_owned()),
        None => Standing::Unknown(
            "the system does not show its environment, and the identity that was recorded of \
             the session's process is not enough to tell it from another process"
                .to_owned(),
        ),
    }
}

/// The processes whose command line holds `root` and whose environment carries `nonce`: what a
/// session started, found without knowing its id. A process that merely has `root` among its
/// arguments, as a tool somebody ran against the run copy does, is not among them.
///
/// On Linux the system lists every thread of a process as an entry of its own, with the
/// process's command line and environment; those entries are left out, so each process is found
/// once, by its process id.
pub(super) fn carrying(root: &Path, nonce: &str) -> Vec<u32> {
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
    );
    let candidates: Vec<Pid> = system
        .processes()
        .iter()
        .filter(|(_, process)| {
            process.thread_kind().is_none()
                && process.status() != ProcessStatus::Zombie
                && process
                    .cmd()
                    .iter()
                    .any(|argument| Path::new(argument) == root)
        })
        .map(|(pid, _)| *pid)
        .collect();
    if candidates.is_empty() {
        return Vec::new();
    }
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&candidates),
        false,
        ProcessRefreshKind::nothing().with_environ(UpdateKind::Always),
    );
    candidates
        .into_iter()
        .filter(|pid| {
            system
                .process(*pid)
                .is_some_and(|process| holds(process.environ(), nonce))
        })
        .map(Pid::as_u32)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    use super::*;
    use crate::{safety::kill_process_group, tui::testing::Program};

    const NONCE: &str = "0123456789abcdef0123456789abcdef";
    /// When the processes of the pure tests started, in seconds since the epoch.
    const STARTED: u64 = 1_790_000_000;

    /// Waits until `condition` holds, for at most ten seconds.
    fn until(what: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !condition() {
            assert!(Instant::now() < deadline, "gave up waiting for {what}");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// What the system shows of a process that started at `started` and runs `/bin/excise`, with
    /// the environment given, or none when the system does not show one.
    fn seen(started: u64, environment: Option<&[&str]>) -> Seen {
        Seen {
            started,
            exe: Some("/bin/excise".to_owned()),
            environment: environment.map(|entries| entries.iter().map(OsString::from).collect()),
        }
    }

    fn recorded(pid: u32, started: u64, exe: Option<&str>) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            started,
            exe: exe.map(str::to_owned),
        }
    }

    #[test]
    fn a_nonce_is_32_hexadecimal_digits_and_never_repeats() {
        let first = new_nonce().expect("a nonce");
        let second = new_nonce().expect("a nonce");

        assert_eq!(first.len(), 32);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
        assert_ne!(first, second);
    }

    #[test]
    fn the_mark_in_the_environment_makes_a_process_ours_when_nothing_recorded_contradicts_it() {
        let mark = format!("{NONCE_VARIABLE}={NONCE}");
        let environment = ["HOME=/tmp/x", mark.as_str()];

        assert_eq!(
            weigh(7, &seen(STARTED, Some(&environment)), None, NONCE),
            Standing::Ours
        );
        let genuine = recorded(7, STARTED, Some("/bin/excise"));
        assert_eq!(
            weigh(7, &seen(STARTED, Some(&environment)), Some(&genuine), NONCE),
            Standing::Ours
        );
    }

    #[test]
    fn a_process_that_took_over_the_id_is_another_whatever_it_carries() {
        // The identity that was recorded is held against the process, and wins over the mark: a
        // process that started at another time, or runs another program, or is named by another
        // id, is not the one that was recorded.
        let mark = format!("{NONCE_VARIABLE}={NONCE}");
        let environment = [mark.as_str()];
        let process = seen(STARTED, Some(&environment));
        let earlier = recorded(7, STARTED - 3_600, Some("/bin/excise"));
        let elsewhere = recorded(7, STARTED, Some("/usr/bin/not-this"));
        let other_id = recorded(8, STARTED, Some("/bin/excise"));

        assert!(matches!(
            weigh(7, &process, Some(&earlier), NONCE),
            Standing::Another(why) if why.contains("started")
        ));
        assert!(matches!(
            weigh(7, &process, Some(&elsewhere), NONCE),
            Standing::Another(why) if why.contains("runs")
        ));
        assert!(matches!(
            weigh(7, &process, Some(&other_id), NONCE),
            Standing::Another(why) if why.contains("process 8")
        ));
    }

    #[test]
    fn a_process_without_the_mark_is_another_even_when_the_identity_matches() {
        // A command line can be copied; an environment that was set up before the process
        // started cannot.
        let genuine = recorded(7, STARTED, Some("/bin/excise"));
        let another_sessions = format!("{NONCE_VARIABLE}=another-nonce");
        let environment = [another_sessions.as_str()];

        assert!(matches!(
            weigh(7, &seen(STARTED, Some(&environment)), Some(&genuine), NONCE),
            Standing::Another(why) if why.contains("mark")
        ));
        assert!(matches!(
            weigh(7, &seen(STARTED, Some(&["HOME=/tmp/x"])), None, NONCE),
            Standing::Another(why) if why.contains("mark")
        ));
    }

    #[test]
    fn what_the_system_does_not_show_is_never_ours_whatever_was_recorded() {
        let genuine = recorded(7, STARTED, Some("/bin/excise"));
        let no_executable = recorded(7, STARTED, None);
        let unreadable = seen(STARTED, None);
        let hidden = Seen {
            exe: None,
            ..seen(STARTED, None)
        };

        // A start time in whole seconds and a path are shared by any process that takes the id in
        // the same second and runs the same program, so a match proves nothing.
        for held in [Some(&genuine), Some(&no_executable), None] {
            for reading in [&unreadable, &hidden] {
                assert!(
                    matches!(weigh(7, reading, held, NONCE), Standing::Unknown(_)),
                    "{held:?} against {:?}",
                    reading.environment
                );
            }
        }
        // A mismatch still rules a process out.
        assert!(matches!(
            weigh(7, &seen(STARTED + 1, None), Some(&genuine), NONCE),
            Standing::Another(why) if why.contains("started")
        ));
        let elsewhere = Seen {
            exe: Some("/usr/bin/other".to_owned()),
            ..seen(STARTED, None)
        };
        assert!(matches!(
            weigh(7, &elsewhere, Some(&genuine), NONCE),
            Standing::Another(why) if why.contains("runs")
        ));
    }

    #[test]
    fn a_process_that_is_being_started_is_never_ours() {
        // Between its creation and its program being loaded, the system shows a process as its
        // parent: the parent's executable and environment, and a moment later no environment.
        let parents_environment = ["HOME=/tmp/x", "PATH=/usr/bin"];
        let as_its_parent = Seen {
            exe: Some("/usr/bin/cargo".to_owned()),
            ..seen(STARTED + 40, Some(&parents_environment))
        };
        let nothing_yet = Seen {
            exe: None,
            ..seen(STARTED, None)
        };
        let genuine = recorded(7, STARTED, Some("/bin/excise"));

        assert!(matches!(
            weigh(7, &as_its_parent, Some(&genuine), NONCE),
            Standing::Another(_)
        ));
        assert!(matches!(
            weigh(7, &as_its_parent, None, NONCE),
            Standing::Another(why) if why.contains("mark")
        ));
        assert!(matches!(
            weigh(7, &nothing_yet, Some(&genuine), NONCE),
            Standing::Unknown(_)
        ));
        assert!(matches!(
            weigh(7, &nothing_yet, None, NONCE),
            Standing::Unknown(_)
        ));
        // A process that a process of the session is starting shows the session's mark, as its
        // parent's environment: it descends from the session's program, so it is the session's.
        let mark = format!("{NONCE_VARIABLE}={NONCE}");
        assert_eq!(
            weigh(9, &seen(STARTED, Some(&[mark.as_str()])), None, NONCE),
            Standing::Ours
        );
    }

    #[test]
    fn an_identity_is_recorded_only_from_a_process_that_shows_the_mark() {
        let root = Path::new("/tmp/xh-tui-recorded/run");
        let ours = Program::start(root, Some(NONCE));
        let without_the_mark = Program::start(root, None);

        let genuine = ProcessIdentity::once_marked(ours.pid(), NONCE, Duration::from_secs(10))
            .expect("a process that shows the mark has an identity");

        assert_eq!(genuine.pid, ours.pid());
        assert!(genuine.started > 0);
        let test_executable = std::env::current_exe().expect("the test executable");
        assert_eq!(
            genuine
                .exe
                .as_deref()
                .map(Path::new)
                .and_then(Path::file_name),
            test_executable.file_name()
        );
        // A process without the mark has none, however long it is looked at.
        assert_eq!(
            ProcessIdentity::once_marked(without_the_mark.pid(), NONCE, Duration::from_millis(200)),
            None
        );
    }

    #[test]
    fn a_real_process_is_weighed_by_what_the_system_shows_of_it() {
        let root = Path::new("/tmp/xh-tui-weighed/run");
        let ours = Program::start(root, Some(NONCE));
        let a_tool_run_against_the_root = Program::start(root, None);
        let another_sessions = Program::start(root, Some("another-nonce"));

        // The system shows a process that was just started as its parent, or without an
        // environment, for a moment (see the module's note on those): each is weighed once it
        // shows what it is.
        until("the mark to be shown", || {
            standing(ours.pid(), None, NONCE) == Standing::Ours
        });
        let genuine = ProcessIdentity::once_marked(ours.pid(), NONCE, Duration::from_secs(10))
            .expect("an identity");
        assert_eq!(standing(ours.pid(), Some(&genuine), NONCE), Standing::Ours);
        for other in [&a_tool_run_against_the_root, &another_sessions] {
            until("the environment to be shown", || {
                !matches!(standing(other.pid(), None, NONCE), Standing::Unknown(_))
            });
            assert!(matches!(
                standing(other.pid(), None, NONCE),
                Standing::Another(why) if why.contains("mark")
            ));
        }
    }

    #[test]
    fn a_process_that_is_not_there_is_gone() {
        let mut quick = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .expect("sh starts");
        let pid = quick.id();
        quick.wait().expect("sh ends");

        assert_eq!(standing(pid, None, NONCE), Standing::Gone);
        assert_eq!(
            ProcessIdentity::once_marked(pid, NONCE, Duration::from_millis(50)),
            None
        );
        assert!(!exists(pid));
    }

    #[test]
    fn a_running_process_exists_until_it_is_killed_and_reaped() {
        let program = Program::start(Path::new("/tmp/xh-tui-exists/run"), None);
        let pid = program.pid();
        assert!(exists(pid));

        kill_process_group(pid).expect("a kill");

        until("the process to be gone", || !exists(pid));
        assert!(!program.running());
    }

    #[test]
    fn processes_are_found_by_their_mark_and_not_by_their_arguments() {
        let root = Path::new("/tmp/xh-tui-feedbeef/run");
        let ours = Program::start(root, Some("the-session-nonce"));
        let a_tool_run_against_the_root = Program::start(root, None);
        let another_sessions_process = Program::start(root, Some("another-nonce"));
        let marked_but_elsewhere =
            Program::start(Path::new("/somewhere/else"), Some("the-session-nonce"));

        // The system lists a new process a moment after it starts.
        until("the mark to be found", || {
            carrying(root, "the-session-nonce").contains(&ours.pid())
        });
        let found = carrying(root, "the-session-nonce");

        assert_eq!(found, [ours.pid()]);
        for other in [
            &a_tool_run_against_the_root,
            &another_sessions_process,
            &marked_but_elsewhere,
        ] {
            assert!(!found.contains(&other.pid()));
        }
    }
}
