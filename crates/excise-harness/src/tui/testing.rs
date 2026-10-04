//! Processes for the tests of this module to weigh, find, and kill.
//!
//! A process the tests start has to be one whose environment the system shows. On macOS it does
//! not show the environment of a system program such as `/bin/sh`, so the tests start the test
//! executable itself, which waits to be killed ([`wait_to_be_killed`]).

use std::{
    env,
    os::unix::process::CommandExt as _,
    path::Path,
    process::{Command, Stdio},
    thread::{self, JoinHandle},
    time::Duration,
};

use super::identity::{self, NONCE_VARIABLE};
use crate::safety::{kill_process, kill_process_group};

/// What a [`Program`] runs: this test executable, asked to run only this test, which waits. It is
/// ignored so that a test run never runs it by itself.
#[test]
#[ignore = "a process that other tests start and kill; it only waits"]
fn wait_to_be_killed() {
    thread::sleep(Duration::from_mins(5));
}

/// A process with `root` among its arguments, that lives until the test ends and is killed when
/// it does, so that a failing test leaves nothing behind.
///
/// Like the program of a session whose supervisor is gone, nothing waits for it while it runs: a
/// thread reaps it the moment it ends, because a process group whose leader nobody has reaped
/// still counts as there.
pub(super) struct Program {
    pid: u32,
    reaper: Option<JoinHandle<()>>,
}

impl Program {
    /// A process that leads a process group of its own, with the session's nonce in its
    /// environment when it is given one, as the program of a session would.
    pub(super) fn start(root: &Path, nonce: Option<&str>) -> Self {
        let mut command = Self::command(root);
        if let Some(nonce) = nonce {
            command.env(NONCE_VARIABLE, nonce);
        }
        Self::spawn(command, 0)
    }

    /// A process in the process group that `leader` leads, with the session's nonce.
    pub(super) fn start_in_group_of(leader: &Self, root: &Path, nonce: &str) -> Self {
        let mut command = Self::command(root);
        command.env(NONCE_VARIABLE, nonce);
        Self::spawn(command, i32::try_from(leader.pid).expect("a process id"))
    }

    /// The test executable, asked to run [`wait_to_be_killed`] and nothing else. `root` is one more
    /// argument, which the test harness reads as a filter that no test matches.
    fn command(root: &Path) -> Command {
        let mut command = Command::new(env::current_exe().expect("the test executable"));
        command
            .args([
                "--ignored",
                "--exact",
                "--nocapture",
                "tui::testing::wait_to_be_killed",
            ])
            .arg(root);
        command
    }

    fn spawn(mut command: Command, group: i32) -> Self {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(group);
        let mut child = command.spawn().expect("a process");
        let pid = child.id();
        let reaper = thread::spawn(move || {
            let _ = child.wait();
        });
        Self {
            pid,
            reaper: Some(reaper),
        }
    }

    pub(super) fn pid(&self) -> u32 {
        self.pid
    }

    /// Whether the process is there and is not a zombie.
    pub(super) fn running(&self) -> bool {
        identity::exists(self.pid)
    }
}

impl Drop for Program {
    fn drop(&mut self) {
        if let Some(reaper) = self.reaper.take() {
            if !reaper.is_finished() {
                let _ = kill_process_group(self.pid);
                let _ = kill_process(self.pid);
            }
            let _ = reaper.join();
        }
    }
}
