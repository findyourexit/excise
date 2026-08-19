#![allow(
    clippy::unnested_or_patterns,
    clippy::option_if_let_else,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

#[cfg(test)]
mod tests;

mod app;
mod input;
mod messages;
mod os;
mod state;
mod ui;

use ::jwalk::Parallelism::{RayonDefaultPool, Serial};
use ::jwalk::WalkDir;
use ::std::env;
use ::std::io;
use ::std::path::PathBuf;
use ::std::process;
use ::std::sync::Arc;
use ::std::sync::atomic::{AtomicBool, Ordering};
use ::std::sync::mpsc;
use ::std::sync::mpsc::{Receiver, SyncSender};
use ::std::thread::park_timeout;
use ::std::{thread, time};
use clap::Parser;

use crossterm::event::KeyModifiers;
use crossterm::event::{Event as BackEvent, KeyCode, KeyEvent};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::{Backend, CrosstermBackend};

use app::{App, UiMode};
use input::TerminalEvents;
use messages::{Event, Instruction, handle_events};

#[cfg(not(test))]
const SHOULD_SHOW_LOADING_ANIMATION: bool = true;
#[cfg(test)]
const SHOULD_SHOW_LOADING_ANIMATION: bool = false;
#[cfg(not(test))]
const SHOULD_HANDLE_WIN_CHANGE: bool = true;
#[cfg(test)]
const SHOULD_HANDLE_WIN_CHANGE: bool = false;
#[cfg(not(test))]
const SHOULD_SCAN_HD_FILES_IN_MULTIPLE_THREADS: bool = true;
#[cfg(test)]
const SHOULD_SCAN_HD_FILES_IN_MULTIPLE_THREADS: bool = false;

#[derive(Parser, Debug)]
#[command(name = "excise", version)]
pub struct Opt {
    #[arg(name = "folder")]
    /// The folder to scan
    folder: Option<PathBuf>,
    #[arg(short, long)]
    /// Show file sizes rather than their block usage on disk
    apparent_size: bool,
    #[arg(short, long)]
    /// Don't ask for confirmation before deleting
    disable_delete_confirmation: bool,
}

fn main() {
    if let Err(err) = try_main() {
        println!("Error: {err}");
        process::exit(2);
    }
}
fn get_stdout() -> io::Stdout {
    io::stdout()
}

fn try_main() -> anyhow::Result<()> {
    let opts = Opt::parse();
    let stdout = get_stdout();
    enable_raw_mode()?;
    let terminal_backend = CrosstermBackend::new(stdout);
    let terminal_events = TerminalEvents {};
    let folder = match opts.folder {
        Some(folder) => folder,
        None => env::current_dir()?,
    };
    if !folder.is_dir() {
        anyhow::bail!("Folder '{}' does not exist", folder.to_string_lossy());
    }
    start(
        terminal_backend,
        Box::new(terminal_events),
        folder,
        opts.apparent_size,
        opts.disable_delete_confirmation,
    );
    disable_raw_mode()?;
    Ok(())
}

/// Starts the application with the provided backend and configuration
///
/// # Panics
/// Panics if any thread fails to spawn or join
pub fn start<B>(
    terminal_backend: B,
    terminal_events: Box<dyn Iterator<Item = BackEvent> + Send>,
    path: PathBuf,
    show_apparent_size: bool,
    disable_delete_confirmation: bool,
) where
    B: Backend + Send + 'static,
{
    let mut active_threads = vec![];
    let (channels, state) = setup_channels_and_state();

    spawn_event_handler_thread(
        &mut active_threads,
        channels.instruction_sender.clone(),
        channels.event_receiver,
    );
    spawn_input_handler_thread(
        &mut active_threads,
        channels.instruction_sender.clone(),
        state.running.clone(),
        terminal_events,
    );
    spawn_scanner_thread(
        &mut active_threads,
        channels.instruction_sender.clone(),
        state.loaded.clone(),
        path.clone(),
    );

    if SHOULD_SHOW_LOADING_ANIMATION {
        spawn_loading_animation_thread(
            &mut active_threads,
            channels.instruction_sender,
            state.running.clone(),
            state.loaded.clone(),
        );
    }

    let mut app = App::new(
        terminal_backend,
        path,
        channels.event_sender,
        show_apparent_size,
        disable_delete_confirmation,
    );
    app.start(&channels.instruction_receiver);
    state.running.store(false, Ordering::Release);

    for thread_handler in active_threads {
        thread_handler.join().expect("Failed to join thread");
    }
}

struct AppChannels {
    event_sender: SyncSender<Event>,
    event_receiver: Receiver<Event>,
    instruction_sender: SyncSender<Instruction>,
    instruction_receiver: Receiver<Instruction>,
}

struct AppState {
    running: Arc<AtomicBool>,
    loaded: Arc<AtomicBool>,
}

fn setup_channels_and_state() -> (AppChannels, AppState) {
    let (event_sender, event_receiver): (SyncSender<Event>, Receiver<Event>) =
        mpsc::sync_channel(1);
    let (instruction_sender, instruction_receiver): (
        SyncSender<Instruction>,
        Receiver<Instruction>,
    ) = mpsc::sync_channel(100);
    let running = Arc::new(AtomicBool::new(true));
    let loaded = Arc::new(AtomicBool::new(false));

    let channels = AppChannels {
        event_sender,
        event_receiver,
        instruction_sender,
        instruction_receiver,
    };

    let state = AppState { running, loaded };

    (channels, state)
}

fn spawn_event_handler_thread(
    active_threads: &mut Vec<thread::JoinHandle<()>>,
    instruction_sender: SyncSender<Instruction>,
    event_receiver: Receiver<Event>,
) {
    active_threads.push(
        thread::Builder::new()
            .name("event_executer".to_string())
            .spawn(|| handle_events(event_receiver, instruction_sender))
            .expect("Failed to spawn thread"),
    );
}

fn spawn_input_handler_thread(
    active_threads: &mut Vec<thread::JoinHandle<()>>,
    instruction_sender: SyncSender<Instruction>,
    running: Arc<AtomicBool>,
    terminal_events: Box<dyn Iterator<Item = BackEvent> + Send>,
) {
    active_threads.push(
        thread::Builder::new()
            .name("stdin_handler".to_string())
            .spawn(move || {
                for evt in terminal_events {
                    if let BackEvent::Resize(_x, _y) = evt {
                        if SHOULD_HANDLE_WIN_CHANGE {
                            let _ = instruction_sender.send(Instruction::ResetUiMode);
                            let _ = instruction_sender.send(Instruction::Render);
                        }
                        continue;
                    }

                    if let BackEvent::Key(KeyEvent {
                        code: KeyCode::Char('y'),
                        modifiers: KeyModifiers::NONE,
                        ..
                    })
                    | BackEvent::Key(KeyEvent {
                        code: KeyCode::Char('q'),
                        modifiers: KeyModifiers::NONE,
                        ..
                    })
                    | BackEvent::Key(KeyEvent {
                        code: KeyCode::Char('Q'),
                        modifiers: KeyModifiers::SHIFT,
                        ..
                    })
                    | BackEvent::Key(KeyEvent {
                        code: KeyCode::Char('c'),
                        modifiers: KeyModifiers::CONTROL,
                        ..
                    }) = evt
                    {
                        let _ = instruction_sender.send(Instruction::Keypress(evt));
                        park_timeout(time::Duration::from_millis(100));
                        if !running.load(Ordering::Acquire) {
                            break;
                        }
                    } else if instruction_sender.send(Instruction::Keypress(evt)).is_err() {
                        break;
                    }
                }
            })
            .expect("Failed to spawn thread"),
    );
}

fn spawn_scanner_thread(
    active_threads: &mut Vec<thread::JoinHandle<()>>,
    instruction_sender: SyncSender<Instruction>,
    loaded: Arc<AtomicBool>,
    path: PathBuf,
) {
    active_threads.push(
        thread::Builder::new()
            .name("hd_scanner".to_string())
            .spawn(move || {
                'scanning: for entry in WalkDir::new(&path)
                    .parallelism(if SHOULD_SCAN_HD_FILES_IN_MULTIPLE_THREADS {
                        RayonDefaultPool {
                            busy_timeout: std::time::Duration::from_millis(100),
                        }
                    } else {
                        Serial
                    })
                    .skip_hidden(false)
                    .follow_links(false)
                {
                    let instruction_sent = match entry {
                        Ok(entry) => match entry.metadata() {
                            Ok(file_metadata) => {
                                let entry_path = entry.path();
                                instruction_sender.send(Instruction::AddEntryToBaseFolder((
                                    file_metadata,
                                    entry_path,
                                )))
                            }
                            Err(_) => instruction_sender.send(Instruction::IncrementFailedToRead),
                        },
                        Err(_) => instruction_sender.send(Instruction::IncrementFailedToRead),
                    };
                    if instruction_sent.is_err() {
                        break 'scanning;
                    }
                }
                let _ = instruction_sender.send(Instruction::StartUi);
                loaded.store(true, Ordering::Release);
            })
            .expect("Failed to spawn thread"),
    );
}

fn spawn_loading_animation_thread(
    active_threads: &mut Vec<thread::JoinHandle<()>>,
    instruction_sender: SyncSender<Instruction>,
    running: Arc<AtomicBool>,
    loaded: Arc<AtomicBool>,
) {
    active_threads.push(
        thread::Builder::new()
            .name("loading_loop".to_string())
            .spawn(move || {
                while running.load(Ordering::Acquire) && !loaded.load(Ordering::Acquire) {
                    let _ = instruction_sender.send(Instruction::ToggleScanningVisualIndicator);
                    let _ = instruction_sender.send(Instruction::RenderAndUpdateBoard);
                    park_timeout(time::Duration::from_millis(100));
                }
            })
            .expect("Failed to spawn thread"),
    );
}
