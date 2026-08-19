use std::io::{Read, Write};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, PtySize, native_pty_system};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);
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

    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_excise"));
    command.arg(fixture.path());
    command.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(command)
        .context("failed to launch Excise in pseudo-terminal")?;
    let mut child_guard = ChildGuard::new(child.clone_killer());
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("failed to clone pseudo-terminal reader")?;
    let writer = Arc::new(Mutex::new(
        pair.master
            .take_writer()
            .context("failed to take pseudo-terminal writer")?,
    ));

    let output = Arc::new((Mutex::new(Vec::new()), Condvar::new()));
    let reader_output = output.clone();
    let reader_writer = writer.clone();
    let reader_thread = thread::spawn(move || -> std::io::Result<()> {
        let mut chunk = [0_u8; 4096];
        let mut answered_queries = 0;
        loop {
            match reader.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(bytes_read) => {
                    let (bytes, changed) = &*reader_output;
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
                        let mut writer = reader_writer.lock().expect("failed to lock PTY writer");
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
    });

    wait_for_output(&output, b"Total:", STARTUP_TIMEOUT)?;
    write_input(&writer, b"q")?;
    wait_for_output(&output, b"Are you sure you want to quit?", STARTUP_TIMEOUT)?;
    write_input(&writer, b"y")?;
    drop(writer);

    let status = wait_for_child(child, EXIT_TIMEOUT)?;
    child_guard.disarm();

    drop(pair.master);
    reader_thread
        .join()
        .map_err(|_| anyhow::anyhow!("PTY reader thread panicked"))?
        .context("failed to read pseudo-terminal output")?;

    let bytes = output.0.lock().expect("failed to lock captured PTY output");
    let rendered = String::from_utf8_lossy(&bytes);
    if !status.success() {
        bail!("Excise exited unsuccessfully: {status}; captured {rendered:?}");
    }
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

    Ok(())
}

fn write_input(writer: &Arc<Mutex<Box<dyn Write + Send>>>, input: &[u8]) -> anyhow::Result<()> {
    let mut writer = writer.lock().expect("failed to lock PTY writer");
    writer
        .write_all(input)
        .context("failed to send pseudo-terminal input")?;
    writer.flush().context("failed to flush PTY input")
}

fn wait_for_output(
    output: &Arc<(Mutex<Vec<u8>>, Condvar)>,
    needle: &[u8],
    timeout: Duration,
) -> anyhow::Result<()> {
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
