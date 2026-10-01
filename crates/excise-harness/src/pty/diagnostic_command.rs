//! A user-configured diagnostic command, run against a session's child process.
//!
//! Some hangs are easiest to explain from the child's own thread stacks: a worker blocked in a
//! system call never reports anything on the event channel, and no amount of output-side
//! diagnostics can say where it is stuck. [`sample`] runs the command named by
//! `EXCISE_HARNESS_DIAGNOSTIC_COMMAND` against the child's pid, non-invasively (it never signals,
//! suspends, or otherwise disturbs the child), and returns its output for a timed-out step's
//! failure detail and the bundle's `screen.txt`.
//!
//! Unset, this costs one environment lookup and nothing else. Set, for example, to
//! `cdb -pv -p {pid} -c "~*k 40; qd"` on Windows (Debugging Tools for Windows) or
//! `sample {pid} 2` on macOS, every `{pid}` in the command is replaced with the child's process
//! id before it runs. The command is parsed as a POSIX-shell-like word list (quoting the way the
//! `cdb` example above does keeps `~*k 40; qd` one argument), run directly (never through a
//! shell), and bounded: if it runs longer than [`COMMAND_TIMEOUT`] it is killed (on Unix, with
//! every process it started in its process group) and reported as timed out, so a broken or
//! hanging diagnostic command can never turn into a second hang. Its output is read while it
//! runs, so a long report never blocks it on a full pipe, and up to [`MAX_OUTPUT_BYTES`] is kept.
//! A process it leaves behind holding the output open is waited for at most [`OUTPUT_GRACE`].

use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

/// How long the configured command may run before it is killed and reported as timed out.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
/// How often a running command is polled for its exit while bounded by [`COMMAND_TIMEOUT`].
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// How long to wait for the output pipes to close once the command has exited or been killed. A
/// process it started outside its process group can keep them open; what arrived by then is kept.
const OUTPUT_GRACE: Duration = Duration::from_millis(500);
/// The most output bytes kept from the command's combined stdout and stderr.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

const ENVIRONMENT_VARIABLE: &str = "EXCISE_HARNESS_DIAGNOSTIC_COMMAND";

/// Runs the command named by `EXCISE_HARNESS_DIAGNOSTIC_COMMAND` against `pid`, substituting
/// every `{pid}` in it, and returns a header naming the command plus its captured output. Returns
/// `None` when the variable is unset. A command that cannot be parsed, spawned, or that overruns
/// its bound reports that in the text it returns rather than failing the caller: this is evidence
/// for a human reading a failure, never something a scenario can depend on.
pub(crate) fn sample(pid: u32) -> Option<String> {
    let template = std::env::var(ENVIRONMENT_VARIABLE).ok()?;
    Some(run(&template, pid, COMMAND_TIMEOUT))
}

fn run(template: &str, pid: u32, timeout: Duration) -> String {
    let words = match shell_words::split(template) {
        Ok(words) if !words.is_empty() => words,
        Ok(_) => return format!("{ENVIRONMENT_VARIABLE} is set but names no command"),
        Err(error) => return format!("{ENVIRONMENT_VARIABLE} could not be parsed: {error}"),
    };
    let pid_text = pid.to_string();
    let mut words = words
        .into_iter()
        .map(|word| word.replace("{pid}", &pid_text));
    let program = words.next().expect("checked non-empty above");
    let arguments: Vec<String> = words.collect();

    let mut command = Command::new(&program);
    command
        .args(&arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;

        // A group of its own, so a timeout reaches every process the command started.
        command.process_group(0);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return format!("could not run `{template}` (resolved program `{program}`): {error}");
        }
    };

    let (closed, pipes_closed) = mpsc::channel();
    let stdout = capture(child.stdout.take(), &closed);
    let stderr = capture(child.stderr.take(), &closed);
    drop(closed);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break None;
                }
                thread::sleep(POLL_INTERVAL);
            }
            Err(_) => break None,
        }
    };
    if status.is_none() {
        #[cfg(unix)]
        {
            let _ = crate::safety::kill_process_group(child.id());
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    let grace_deadline = Instant::now() + OUTPUT_GRACE;
    let mut open_pipes = 2_u8;
    while open_pipes > 0
        && pipes_closed
            .recv_timeout(grace_deadline.saturating_duration_since(Instant::now()))
            .is_ok()
    {
        open_pipes -= 1;
    }

    let mut output = kept_bytes(&stdout);
    let errors = kept_bytes(&stderr);
    let room = MAX_OUTPUT_BYTES.saturating_sub(output.len());
    output.extend_from_slice(&errors[..errors.len().min(room)]);
    let captured = String::from_utf8_lossy(&output);

    let ended = match status {
        Some(status) => format!("`{template}` exited with {status}"),
        None => format!("`{template}` did not finish within {timeout:?} and was killed"),
    };
    let header = if open_pipes == 0 {
        ended
    } else {
        format!(
            "{ended}; its output was still open {OUTPUT_GRACE:?} later, so this is what had \
             arrived"
        )
    };
    format!("{header}\n{captured}")
}

/// Reads `pipe` to its end on a thread of its own, keeping at most [`MAX_OUTPUT_BYTES`] and
/// discarding the rest, so that the command never blocks on a full pipe. Sends on `closed` when
/// the pipe ends or fails. Without a pipe, or a thread to read it, the returned buffer stays
/// empty and nothing is sent; the caller's bounded wait covers that.
fn capture(pipe: Option<impl Read + Send + 'static>, closed: &Sender<()>) -> Arc<Mutex<Vec<u8>>> {
    let kept = Arc::new(Mutex::new(Vec::new()));
    if let Some(mut pipe) = pipe {
        let sink = Arc::clone(&kept);
        let closed = closed.clone();
        let _ = thread::Builder::new()
            .name("excise-harness-diagnostic-output".to_owned())
            .spawn(move || {
                let mut chunk = [0_u8; 8 * 1024];
                loop {
                    match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(read) => {
                            let mut kept = sink.lock().unwrap_or_else(PoisonError::into_inner);
                            let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
                            kept.extend_from_slice(&chunk[..read.min(room)]);
                        }
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                        Err(_) => break,
                    }
                }
                let _ = closed.send(());
            });
    }
    kept
}

/// Takes what a [`capture`] buffer holds now. A reader still running (a leftover process holds
/// its pipe) appends to the emptied buffer, which nothing reads again.
fn kept_bytes(kept: &Mutex<Vec<u8>>) -> Vec<u8> {
    std::mem::take(&mut *kept.lock().unwrap_or_else(PoisonError::into_inner))
}

#[cfg(test)]
mod tests {
    use super::*;

    // `sample` only reads the environment variable and forwards to `run`; tests cannot call
    // `std::env::set_var` (unsafe in edition 2024), so every behavior below exercises `run`
    // directly, the way `opt_in_from_value` separates parsing from `std::env::var` elsewhere in
    // this crate. `sample`'s own "unset" path relies on no test runner setting this variable.

    #[test]
    fn unset_reports_nothing() {
        assert!(sample(std::process::id()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn the_pid_placeholder_is_substituted_into_every_argument() {
        let text = run("echo pid={pid}-again", 4242, COMMAND_TIMEOUT);
        assert!(
            text.contains("pid=4242-again"),
            "the command should have seen the substituted pid: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn quoted_arguments_survive_word_splitting() {
        let text = run(
            r#"sh -c "printf '%s' '{pid} together'""#,
            7,
            COMMAND_TIMEOUT,
        );
        assert!(
            text.contains("7 together"),
            "the quoted argument should have stayed one word: {text}"
        );
    }

    #[test]
    fn an_unparsable_command_is_reported_not_panicked() {
        let text = run("'unterminated", 1, COMMAND_TIMEOUT);
        assert!(text.contains("could not be parsed"), "{text}");
    }

    #[test]
    fn a_program_that_cannot_be_found_is_reported_not_panicked() {
        let text = run(
            "excise-harness-diagnostic-command-that-does-not-exist {pid}",
            1,
            COMMAND_TIMEOUT,
        );
        assert!(text.contains("could not run"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn a_command_that_overruns_its_bound_is_killed_with_what_it_started() {
        // `; :` keeps the shell alive, so `sleep` is its child on every shell, as with dash's
        // `sh -c "sleep 30"`; a surviving `sleep` would hold the output pipes open for 30 s.
        let started = Instant::now();
        let text = run("sh -c \"sleep 30; :\"", 1, Duration::from_millis(200));
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the bound should have cut the wait well under the command's own 30 s: {text}"
        );
        assert!(text.contains("did not finish within"), "{text}");
    }

    #[cfg(unix)]
    #[test]
    fn output_beyond_a_pipe_buffer_neither_stalls_the_command_nor_exceeds_the_cap() {
        let text = run(
            "sh -c \"yes 0123456789 | head -c 300000\"",
            1,
            Duration::from_secs(10),
        );
        let (header, captured) = text.split_once('\n').unwrap_or((&text, ""));
        assert!(header.contains("exited with"), "{header}");
        assert_eq!(captured.len(), MAX_OUTPUT_BYTES, "{header}");
    }

    #[cfg(unix)]
    #[test]
    fn a_process_left_holding_the_output_does_not_extend_the_wait() {
        // `set -m` starts the background `sleep` in a process group of its own, so it outlives
        // the command and keeps the output pipes open after the command has exited.
        let started = Instant::now();
        let text = run(
            "sh -c \"set -m; sleep 10 & echo started\"",
            1,
            COMMAND_TIMEOUT,
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a pipe held open by a leftover process should not hold the caller: {text}"
        );
        assert!(text.contains("exited with"), "{text}");
        assert!(text.contains("started"), "{text}");
        assert!(text.contains("still open"), "{text}");
    }
}
