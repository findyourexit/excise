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

#[cfg(feature = "internal")]
pub mod animation;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod animation;
#[allow(dead_code)]
mod app;
#[cfg(feature = "internal")]
pub mod benchmark;
#[allow(dead_code)]
mod cli;
#[cfg(feature = "internal")]
pub mod config;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod config;
#[cfg(feature = "internal")]
pub mod deletion;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod deletion;
#[cfg(feature = "internal")]
pub mod error;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod error;
mod file_id_codec;
#[cfg(feature = "internal")]
pub mod filter;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod filter;
#[cfg(feature = "internal")]
pub mod input;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod input;
#[cfg(feature = "internal")]
pub mod model;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod model;
#[cfg(feature = "internal")]
pub mod native_path;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod native_path;
#[allow(dead_code)]
mod os;
#[cfg(feature = "internal")]
pub mod outcome;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod outcome;
#[cfg(feature = "internal")]
pub mod report;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod report;
#[cfg(feature = "internal")]
pub mod runtime;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod runtime;
#[allow(dead_code)]
mod scan_coordinator;
mod scan_session;
#[allow(dead_code)]
mod scan_store;
#[allow(dead_code)]
mod state;
mod temporary_storage;
#[cfg(feature = "internal")]
pub mod terminal;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod terminal;
#[cfg(feature = "internal")]
pub mod theme;
#[cfg(not(feature = "internal"))]
#[allow(dead_code)]
mod theme;
#[allow(dead_code)]
mod ui;

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

#[cfg(feature = "internal")]
pub mod geometry {
    pub use crate::state::tiles::{FileMetadata, FileType, HALF_ROWS_PER_CELL, Tile, TreeMap};
}
#[cfg(feature = "fuzzing")]
pub mod fuzz {
    pub use crate::state::FileToDelete;
    pub use crate::terminal::{TerminalState, TerminalTransition};

    pub mod animation {
        pub use crate::animation::{ACTIVE_FRAME_INTERVAL, AnimationScheduler};
    }

    pub mod config {
        pub use crate::config::{
            Cli, EnvironmentOverrides, KeyPreset, RuntimeConfig, parse_file_config,
        };
    }

    pub mod deletion {
        pub use crate::deletion::{
            DeletionEntryOutcome, DeletionEntryResult, DeletionPlanError, DeletionReport,
            PlannedEntry, PlannedKind, PlannedSnapshot, ReviewedEntry, build_plan_cancellable,
            execute_plan,
        };
    }

    pub mod error {
        pub use crate::error::AppError;
    }

    pub mod filter {
        pub use crate::filter::FilterPattern;
    }

    pub mod geometry {
        pub use crate::state::tiles::{FileMetadata, FileType, HALF_ROWS_PER_CELL, TreeMap};
    }

    pub mod input {
        pub use crate::input::{InputEvent, InputSource};
    }

    pub mod model {
        pub use crate::model::{ByteBounds, DEFAULT_PROCESS_MIB, EntrySnapshot, NodeId, NodeKind};
    }

    pub mod scan_store {
        /// Reduces generated identity facts through the canonical streaming reducer.
        #[must_use]
        pub fn reduce_identity_bytes(data: &[u8]) -> usize {
            crate::scan_store::identity_observation::fuzz_reduce_identity_bytes(data)
        }
    }

    pub mod native_path {
        pub use crate::native_path::{NativeIdentity, NativePath, identity_for};
    }

    pub mod report {
        pub use crate::report::{DeletionHistoryDocument, ScanReportDocument};
    }

    pub mod runtime {
        pub use crate::runtime::{RuntimeSettings, VirtualClock, run};
    }

    pub mod theme {
        pub use crate::theme::ThemeId;
    }
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
        temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
        scan_store_mib: Some(4_096),
        scan_store_reserve_mib: None,
        scan_store_dir: None,
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
