//! Excise's supported product is the `excise` command-line program.
//!
//! The library target shares private implementation modules with the binary. It is not a
//! supported Rust API. Use the command-line tool and its documented configuration and report contracts.

#![allow(
    clippy::unnested_or_patterns,
    clippy::option_if_let_else,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod animation;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod animation;
#[allow(dead_code)]
mod app;
#[allow(dead_code)]
mod cli;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod config;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod config;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod deletion;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod deletion;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod error;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod error;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod filter;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod filter;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod input;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod input;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod model;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod model;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod native_path;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod native_path;
#[allow(dead_code)]
mod os;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod outcome;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod outcome;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod report;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod report;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod runtime;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod runtime;
#[allow(dead_code)]
mod state;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod terminal;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod terminal;
#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod theme;
#[cfg(not(any(feature = "fuzzing", feature = "internal")))]
#[allow(dead_code)]
mod theme;
#[allow(dead_code)]
mod ui;
#[cfg(windows)]
mod windows_delete;

pub(crate) use app::{App, UiMode};

/// Internal entry point used by the binary target.
#[doc(hidden)]
#[must_use]
pub fn run_main() -> i32 {
    cli::run_main()
}

/// Internal command factory used to regenerate the CLI's distribution artifacts.
#[doc(hidden)]
#[must_use]
pub fn cli_command() -> clap::Command {
    use clap::CommandFactory as _;

    config::Cli::command()
}

#[cfg(any(feature = "fuzzing", feature = "internal"))]
pub mod geometry {
    pub use crate::state::tiles::{FileMetadata, FileType, HALF_ROWS_PER_CELL, Tile, TreeMap};
}
#[cfg(feature = "fuzzing")]
pub use state::FileToDelete;
#[cfg(feature = "fuzzing")]
pub use terminal::{TerminalState, TerminalTransition};

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
