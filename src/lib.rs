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
pub mod deletion;
pub mod error;
pub mod filter;
pub mod input;
pub mod model;
pub mod native_path;
mod os;
pub mod outcome;
pub mod report;
pub mod runtime;
mod state;
pub mod terminal;
pub mod theme;
mod ui;
#[cfg(windows)]
mod windows_delete;

pub(crate) use app::{App, UiMode};
pub use input::TerminalEvents;
pub use state::FileToDelete;
pub use terminal::{
    TerminalSession, TerminalState, TerminalTransition, TerminalTransitionError, validate_terminal,
};

pub mod geometry {
    pub use crate::state::tiles::{FileMetadata, FileType, Tile, TreeMap};
}

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
    let metadata = std::fs::symlink_metadata(&path).expect("test root metadata should exist");
    let root_identity = crate::native_path::identity_for(&path, &metadata)
        .expect("test root identity should be readable")
        .expect("test root should not be a symbolic link");
    let settings = runtime::RuntimeSettings {
        root: path,
        root_identity,
        scan_threads: 1,
        event_capacity: 256,
        cross_filesystems: false,
        exclusions: Vec::new(),
        memory_mib: crate::model::DEFAULT_PROCESS_MIB,
        apparent_size: show_apparent_size,
        disable_delete_confirmation,
        reduced_motion: true,
        monochrome: true,
        animate_loading: false,
        theme: crate::theme::ThemeId::ExciseDark,
        ascii: false,
        mouse: false,
        keymap: crate::config::KeyPreset::Vim,
        custom_keys: None,
        config_path: None,
        monochrome_locked: true,
    };
    runtime::run(
        terminal_backend,
        terminal_events,
        settings,
        Box::new(runtime::VirtualClock::new()),
    )
    .expect("test runtime failed");
}
