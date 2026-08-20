use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, PtySize, native_pty_system};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
const FIRST_FRAME_BUDGET: Duration = Duration::from_millis(100);
const INPUT_FRAME_BUDGET: Duration = Duration::from_millis(50);
const NORMAL_QUIT_BUDGET: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
struct PtyMetrics {
    first_frame: Duration,
    input_to_frame: Duration,
    normal_quit: Duration,
}
type SharedOutput = Arc<(Mutex<Vec<u8>>, Condvar)>;
type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;
type FirstOutput = Arc<Mutex<Option<Instant>>>;
struct ChildGuard {
    killer: Box<dyn ChildKiller + Send + Sync>,
    armed: bool,
}

impl ChildGuard {
    fn new(killer: Box<dyn ChildKiller + Send + Sync>) -> Self {
        Self {
            killer,
            armed: true,
        }
    }

    const fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.killer.kill();
        }
    }
}

#[test]
fn launches_renders_accepts_input_and_restores_terminal() -> anyhow::Result<()> {
    let (status, output, metrics) = run_pty_interaction(b"y", false)?;
    if !status.success() {
        bail!("Excise exited unsuccessfully: {status}; captured {output:?}");
    }
    let metrics = metrics.context("normal run did not record PTY metrics")?;
    if std::env::var_os("EXCISE_PTY_BINARY").is_some() {
        assert!(
            metrics.first_frame <= FIRST_FRAME_BUDGET,
            "first frame took {:?}; captured {output:?}",
            metrics.first_frame
        );
        assert!(
            metrics.input_to_frame <= INPUT_FRAME_BUDGET,
            "input-to-frame took {:?}",
            metrics.input_to_frame
        );
        assert!(
            metrics.normal_quit <= NORMAL_QUIT_BUDGET,
            "normal quit took {:?}",
            metrics.normal_quit
        );
    }
    Ok(())
}

#[test]
fn hard_cancel_restores_terminal_and_uses_exit_130() -> anyhow::Result<()> {
    let (status, output, _) = run_pty_interaction(b"\x03", false)?;
    if status.exit_code() != 130 {
        bail!("hard cancel exited with {status}; captured {output:?}");
    }
    Ok(())
}

#[test]
fn panic_restores_terminal_before_diagnostics() -> anyhow::Result<()> {
    let (status, output, _) = run_pty_interaction(&[], true)?;
    if status.exit_code() != 101 {
        bail!("injected panic exited with {status}; captured {output:?}");
    }
    let restored = output
        .rfind("\u{1b}[?1049l")
        .context("panic path never left the alternate screen")?;
    let diagnostic = output
        .find("panicked at")
        .context("panic diagnostic was not emitted")?;
    assert!(
        restored < diagnostic,
        "panic diagnostic preceded terminal restoration"
    );
    Ok(())
}
fn run_pty_interaction(
    exit_input: &[u8],
    panic_after_entry: bool,
) -> anyhow::Result<(ExitStatus, String, Option<PtyMetrics>)> {
    let fixture = tempfile::tempdir().context("failed to create PTY fixture")?;
    std::fs::write(fixture.path().join("smoke-file"), b"excise")
        .context("failed to create PTY fixture file")?;

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("failed to open pseudo-terminal")?;

    let binary = if panic_after_entry {
        std::ffi::OsString::from(env!("CARGO_BIN_EXE_excise"))
    } else {
        std::env::var_os("EXCISE_PTY_BINARY")
            .unwrap_or_else(|| std::ffi::OsString::from(env!("CARGO_BIN_EXE_excise")))
    };
    let mut command = CommandBuilder::new(binary);
    command.arg(fixture.path());
    command.env("TERM", "xterm-256color");

    if panic_after_entry {
        command.env("EXCISE_TEST_PANIC_AFTER_TERMINAL_ENTRY", "1");
    }
    let child = pair
        .slave
        .spawn_command(command)
        .context("failed to launch Excise in pseudo-terminal")?;
    let mut child_guard = ChildGuard::new(child.clone_killer());
    drop(pair.slave);

    let reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone pseudo-terminal reader")?;
    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .context("failed to take pseudo-terminal writer")?,
    ));

    let output = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let first_output = Arc::new(Mutex::new(None));
    let reader_thread =
        spawn_terminal_reader(reader, output.clone(), writer.clone(), first_output.clone());

    let mut measured = None;
    let mut quit_started = None;
    if !panic_after_entry {
        wait_for_output(&output, b"Folder is empty", STARTUP_TIMEOUT)?;
        let terminal_started = first_output
            .lock()
            .expect("failed to lock PTY timing")
            .expect("terminal emitted no output");
        let first_frame = terminal_started.elapsed();
        wait_for_output(&output, b"Total:", STARTUP_TIMEOUT)?;
        let input_started = Instant::now();
        write_input(&writer, b"q")?;
        wait_for_output(&output, b"Are you sure you want to quit?", STARTUP_TIMEOUT)?;
        let input_to_frame = input_started.elapsed();
        quit_started = Some(Instant::now());
        write_input(&writer, exit_input)?;
        measured = Some((first_frame, input_to_frame));
    }
    drop(writer);

    let status = wait_for_child(child, EXIT_TIMEOUT)?;
    let normal_quit = quit_started.map(|started| started.elapsed());
    child_guard.disarm();
    drop(pair.master);
    reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("PTY reader thread panicked"))?
        .context("failed to read pseudo-terminal output")?;

    let bytes = output.0.lock().expect("failed to lock captured PTY output");
    let rendered = String::from_utf8_lossy(&bytes).into_owned();
    if !panic_after_entry {
        let hidden = rendered
            .rfind("\u{1b}[?25l")
            .context("terminal output never hid the cursor")?;
        let shown = rendered
            .rfind("\u{1b}[?25h")
            .context("terminal output never restored the cursor")?;
        assert!(
            shown > hidden,
            "cursor was not visible after terminal cleanup"
        );
    }
    assert!(
        rendered.contains("\u{1b}[?1049h"),
        "alternate screen was never entered"
    );
    assert!(
        rendered.contains("\u{1b}[?1049l"),
        "alternate screen was never left"
    );

    let metrics = measured
        .zip(normal_quit)
        .map(|((first_frame, input_to_frame), normal_quit)| PtyMetrics {
            first_frame,
            input_to_frame,
            normal_quit,
        });
    Ok((status, rendered, metrics))
}

fn spawn_terminal_reader(
    mut reader: Box<dyn Read + Send>,
    output: SharedOutput,
    writer: SharedWriter,
    first_output: FirstOutput,
) -> thread::JoinHandle<std::io::Result<()>> {
    thread::spawn(move || {
        let mut chunk = [0_u8; 4096];
        let mut answered_queries = 0;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(bytes_read) => {
                    first_output
                        .lock()
                        .expect("failed to lock PTY timing")
                        .get_or_insert_with(Instant::now);
                    let (bytes, changed) = &*output;
                    let query_count = {
                        let mut bytes = bytes.lock().expect("failed to lock PTY output");
                        bytes.extend_from_slice(&chunk[..bytes_read]);
                        bytes
                            .windows(b"\x1b[6n".len())
                            .filter(|window| *window == b"\x1b[6n")
                            .count()
                    };
                    changed.notify_all();
                    if query_count > answered_queries {
                        let mut writer = writer.lock().expect("failed to lock PTY writer");
                        for _ in answered_queries..query_count {
                            writer.write_all(b"\x1b[1;1R")?;
                        }
                        writer.flush()?;
                        answered_queries = query_count;
                    }
                }
                Err(error) if error.raw_os_error() == Some(5) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    })
}

#[test]
fn invalid_config_fails_before_terminal_entry() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create config fixture")?;
    let config = fixture.path().join("invalid.toml");
    std::fs::write(&config, "version = 99\n").context("failed to write invalid config")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg("--config")
        .arg(config)
        .output()
        .context("failed to execute invalid-config check")?;
    assert_eq!(output.status.code(), Some(78));
    assert_no_terminal_controls(&output);
    Ok(())
}

#[test]
fn invalid_path_fails_before_terminal_entry() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create path fixture")?;
    let missing = fixture.path().join("missing");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg(missing)
        .output()
        .context("failed to execute invalid-path check")?;
    assert_eq!(output.status.code(), Some(74));
    assert_no_terminal_controls(&output);
    Ok(())
}

#[test]
fn non_tty_fails_without_emitting_terminal_controls() -> anyhow::Result<()> {
    let fixture = tempfile::tempdir().context("failed to create non-TTY fixture")?;
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg(fixture.path())
        .output()
        .context("failed to execute non-TTY check")?;
    assert_eq!(output.status.code(), Some(70));
    assert_no_terminal_controls(&output);
    Ok(())
}

fn assert_no_terminal_controls(output: &std::process::Output) {
    assert!(!output.stdout.contains(&0x1b));
    assert!(!output.stderr.contains(&0x1b));
}

fn write_input(writer: &SharedWriter, input: &[u8]) -> anyhow::Result<()> {
    let mut writer = writer.lock().expect("failed to lock PTY writer");
    writer
        .write_all(input)
        .context("failed to send pseudo-terminal input")?;
    writer.flush().context("failed to flush PTY input")
}

fn wait_for_output(output: &SharedOutput, needle: &[u8], timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let (bytes, changed) = &**output;
    let mut bytes = bytes.lock().expect("failed to lock PTY output");
    loop {
        if bytes.windows(needle.len()).any(|window| window == needle) {
            return Ok(());
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            let captured = String::from_utf8_lossy(&bytes);
            bail!("timed out waiting for initial terminal render; captured {captured:?}");
        };
        let (next, wait) = changed
            .wait_timeout(bytes, remaining)
            .expect("failed to wait for PTY output");
        bytes = next;
        if wait.timed_out() {
            let captured = String::from_utf8_lossy(&bytes);
            bail!("timed out waiting for initial terminal render; captured {captured:?}");
        }
    }
}

fn wait_for_child(
    mut child: Box<dyn Child + Send + Sync>,
    timeout: Duration,
) -> anyhow::Result<ExitStatus> {
    let mut killer = child.clone_killer();
    let (finished, completion) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let _ = finished.send(child.wait());
    });

    match completion.recv_timeout(timeout) {
        Ok(status) => status.context("failed to wait for Excise"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            killer.kill().context("failed to terminate hung Excise")?;
            bail!("timed out waiting for Excise to exit")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("Excise wait thread disconnected")
        }
    }
}
