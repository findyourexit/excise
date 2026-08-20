#![allow(
    clippy::unnested_or_patterns,
    clippy::option_if_let_else,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

pub mod animation;
mod app;
pub mod config;
pub mod error;
pub mod input;
pub mod native_path;
mod os;
pub mod outcome;
pub mod runtime;
mod state;
pub mod terminal;
mod ui;

pub(crate) use app::{App, UiMode};
pub use input::TerminalEvents;
pub use terminal::{
    TerminalSession, TerminalState, TerminalTransition, TerminalTransitionError, validate_terminal,
};

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) fn start<B>(
    terminal_backend: B,
    terminal_events: Box<dyn input::InputSource>,
    path: std::path::PathBuf,
    show_apparent_size: bool,
    disable_delete_confirmation: bool,
) where
    B: ratatui::backend::Backend,
{
    let settings = runtime::RuntimeSettings {
        root: path,
        scan_threads: 1,
        event_capacity: 256,
        apparent_size: show_apparent_size,
        disable_delete_confirmation,
        reduced_motion: true,
        monochrome: true,
        animate_loading: false,
    };
    runtime::run(
        terminal_backend,
        terminal_events,
        settings,
        Box::new(runtime::VirtualClock::new()),
    )
    .expect("test runtime failed");
}
