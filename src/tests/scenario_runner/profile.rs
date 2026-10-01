//! Profiles: the named configurations a scenario runs under, in-process.
//!
//! Each profile is what the process runners do with environment variables, expressed as the same
//! options through the same configuration layers, without reading the environment or any
//! configuration file:
//!
//! | Profile | In-process configuration |
//! |---|---|
//! | `default` | the configuration defaults |
//! | `deterministic` | reduced motion, loading animation off, one scan thread |
//! | `reduced-motion` | reduced motion and loading animation off, thread count unchanged |
//! | `monochrome-ascii` | the monochrome theme, ASCII symbols and borders |
//! | `narrow` | a backend 60 columns wide, with the scenario's rows |
//! | `mouse-keymaps` | mouse input, the Emacs key preset |

use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser as _;
use excise_harness::scenario::{Profile, Terminal};

use crate::config::{Cli, EnvironmentOverrides, RuntimeConfig};
use crate::native_path::identity_for;
use crate::runtime::RuntimeSettings;
use crate::theme::ThemeId;

/// The width of the `narrow` profile's backend.
const NARROW_COLS: u16 = 60;

/// What a run needs from its profile: the runtime settings and the size of the backend.
pub struct Configuration {
    pub settings: RuntimeSettings,
    pub cols: u16,
    pub rows: u16,
}

/// Builds the configuration for `profile` on `root`. `scan_store_dir` is the scratch directory that
/// holds the run's scan-store session, so the run leaves nothing in the shared temporary directory.
///
/// # Errors
///
/// Returns a message when the root cannot be inspected or the options do not resolve.
pub fn configure(
    profile: Profile,
    terminal: Terminal,
    root: &Path,
    scan_store_dir: PathBuf,
) -> Result<Configuration, String> {
    let (options, animate_loading): (&[&str], bool) = match profile {
        Profile::Default | Profile::Narrow => (&[], true),
        Profile::Deterministic => (&["--reduced-motion", "--scan-threads", "1"], false),
        Profile::ReducedMotion => (&["--reduced-motion"], false),
        Profile::MonochromeAscii => (&["--theme", "monochrome", "--ascii"], true),
        Profile::MouseKeymaps => (&["--mouse", "--keymap", "emacs"], true),
    };
    let cli = Cli::try_parse_from(std::iter::once("excise").chain(options.iter().copied()))
        .map_err(|error| error.to_string())?;
    let config = RuntimeConfig::from_layers(
        cli,
        None,
        EnvironmentOverrides::default(),
        root.to_path_buf(),
        None,
    )
    .map_err(|error| error.to_string())?;
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("the fixture root cannot be inspected: {error}"))?;
    let root_identity = identity_for(root, &metadata)
        .map_err(|error| format!("the fixture root has no readable identity: {error}"))?
        .ok_or_else(|| "the fixture root is a symbolic link".to_owned())?;
    let monochrome_locked = config.monochrome && config.theme != ThemeId::Monochrome;
    let settings = RuntimeSettings {
        root: root.to_path_buf(),
        root_identity,
        scan_threads: config.scan_threads,
        event_capacity: config.event_buffer,
        cross_filesystems: config.cross_filesystems,
        exclusions: config.exclusions,
        memory_mib: config.memory_mib,
        temporary_storage_mib: config.temporary_storage_mib,
        scan_store_mib: config.scan_store_mib,
        scan_store_reserve_mib: config.scan_store_reserve_mib,
        scan_store_dir: Some(scan_store_dir),
        apparent_size: config.apparent_size,
        disable_delete_confirmation: false,
        reduced_motion: config.reduced_motion,
        monochrome: config.monochrome,
        animate_loading,
        theme: config.theme,
        ascii: config.ascii,
        mouse: config.mouse,
        keymap: config.keymap,
        custom_keys: config.custom_keys,
        config_path: None,
        monochrome_locked,
    };
    let cols = if profile == Profile::Narrow {
        NARROW_COLS
    } else {
        terminal.cols
    };
    Ok(Configuration {
        settings,
        cols,
        rows: terminal.rows,
    })
}
