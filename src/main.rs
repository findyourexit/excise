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
use input::{InputEvent, TerminalEvents};
use messages::{Event, EventSender, EventTracker, Instruction, handle_events};

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
    terminal_events: Box<dyn Iterator<Item = InputEvent> + Send>,
    path: PathBuf,
    show_apparent_size: bool,
    disable_delete_confirmation: bool,
) where
    B: Backend + Send + 'static,
{
    let mut active_threads = vec![];
    let (channels, state) = setup_channels_and_state();

    let mut app = App::new(
        terminal_backend,
        path.clone(),
        channels.event_sender,
        show_apparent_size,
        disable_delete_confirmation,
    );

    spawn_event_handler_thread(
        &mut active_threads,
        channels.instruction_sender.clone(),
        channels.event_receiver,
        state.event_tracker.clone(),
    );
    spawn_input_handler_thread(
        &mut active_threads,
        channels.instruction_sender.clone(),
        state.event_tracker.clone(),
        terminal_events,
    );
    spawn_scanner_thread(
        &mut active_threads,
        channels.instruction_sender.clone(),
        state.event_tracker.clone(),
        state.loaded.clone(),
        path,
    );

    if SHOULD_SHOW_LOADING_ANIMATION {
        spawn_loading_animation_thread(
            &mut active_threads,
            channels.instruction_sender,
            state.running.clone(),
            state.loaded.clone(),
        );
    }

    app.start(&channels.instruction_receiver);
    state.running.store(false, Ordering::Release);

    for thread_handler in active_threads {
        thread_handler.join().expect("Failed to join thread");
    }
}

struct AppChannels {
    event_sender: EventSender,
    event_receiver: Receiver<Event>,
    instruction_sender: SyncSender<Instruction>,
    instruction_receiver: Receiver<Instruction>,
}

struct AppState {
    running: Arc<AtomicBool>,
    loaded: Arc<AtomicBool>,
    event_tracker: Arc<EventTracker>,
}

fn setup_channels_and_state() -> (AppChannels, AppState) {
    let (raw_event_sender, event_receiver): (SyncSender<Event>, Receiver<Event>) =
        mpsc::sync_channel(1);
    let (instruction_sender, instruction_receiver): (
        SyncSender<Instruction>,
        Receiver<Instruction>,
    ) = mpsc::sync_channel(100);
    let running = Arc::new(AtomicBool::new(true));
    let loaded = Arc::new(AtomicBool::new(false));
    let event_tracker = Arc::new(EventTracker::default());
    event_tracker.begin();
    let event_sender = EventSender::new(raw_event_sender, event_tracker.clone());

    let channels = AppChannels {
        event_sender,
        event_receiver,
        instruction_sender,
        instruction_receiver,
    };

    let state = AppState {
        running,
        loaded,
        event_tracker,
    };

    (channels, state)
}

fn spawn_event_handler_thread(
    active_threads: &mut Vec<thread::JoinHandle<()>>,
    instruction_sender: SyncSender<Instruction>,
    event_receiver: Receiver<Event>,
    event_tracker: Arc<EventTracker>,
) {
    active_threads.push(
        thread::Builder::new()
            .name("event_executer".to_string())
            .spawn(|| handle_events(event_receiver, instruction_sender, event_tracker))
            .expect("Failed to spawn thread"),
    );
}

fn spawn_input_handler_thread(
    active_threads: &mut Vec<thread::JoinHandle<()>>,
    instruction_sender: SyncSender<Instruction>,
    event_tracker: Arc<EventTracker>,
    terminal_events: Box<dyn Iterator<Item = InputEvent> + Send>,
) {
    active_threads.push(
        thread::Builder::new()
            .name("stdin_handler".to_string())
            .spawn(move || {
                for input in terminal_events {
                    let evt = match input {
                        InputEvent::Terminal(evt) => evt,
                        InputEvent::Barrier => {
                            if !synchronize_input(&instruction_sender, &event_tracker) {
                                break;
                            }
                            continue;
                        }
                    };

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
                        let (acknowledgment, processed) = mpsc::sync_channel(0);
                        if instruction_sender
                            .send(Instruction::Keypress(evt, Some(acknowledgment)))
                            .is_err()
                            || !matches!(processed.recv(), Ok(true))
                        {
                            break;
                        }
                    } else if instruction_sender
                        .send(Instruction::Keypress(evt, None))
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("Failed to spawn thread"),
    );
}

fn synchronize_input(
    instruction_sender: &SyncSender<Instruction>,
    event_tracker: &EventTracker,
) -> bool {
    if !synchronize_instructions(instruction_sender) {
        return false;
    }
    event_tracker.wait_until_idle();
    synchronize_instructions(instruction_sender)
}

fn synchronize_instructions(instruction_sender: &SyncSender<Instruction>) -> bool {
    let (acknowledgment, processed) = mpsc::sync_channel(0);
    instruction_sender
        .send(Instruction::Synchronize(acknowledgment))
        .is_ok()
        && processed.recv().is_ok()
}

fn spawn_scanner_thread(
    active_threads: &mut Vec<thread::JoinHandle<()>>,
    instruction_sender: SyncSender<Instruction>,
    event_tracker: Arc<EventTracker>,
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
                event_tracker.complete();
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
