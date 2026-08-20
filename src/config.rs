use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use directories::ProjectDirs;
use serde::Deserialize;

use crate::error::AppError;

pub const CONFIG_VERSION: u16 = 1;
const DEFAULT_EVENT_BUFFER: usize = 256;
const MAX_SCANNER_THREADS: usize = 32;

#[derive(Clone, Debug, Parser)]
#[command(name = "excise", version, disable_help_subcommand = true)]
pub struct Cli {
    #[arg(name = "folder")]
    /// Folder to scan
    pub folder: Option<PathBuf>,
    #[arg(long, value_name = "FILE")]
    /// Read configuration from FILE
    pub config: Option<PathBuf>,
    #[arg(short, long)]
    /// Show apparent file sizes instead of allocated bytes
    pub apparent_size: bool,
    #[arg(long, value_name = "COUNT")]
    /// Scanner worker count (1-32)
    pub scan_threads: Option<usize>,
    #[arg(long, value_name = "COUNT")]
    /// Bounded worker-event capacity (16-4096)
    pub event_buffer: Option<usize>,
    #[arg(long)]
    /// Disable nonessential motion
    pub reduced_motion: bool,
    #[arg(short, long)]
    /// Do not ask for confirmation before deleting
    pub disable_delete_confirmation: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub version: u16,
    #[serde(default)]
    pub scanner: ScannerFileConfig,
    #[serde(default)]
    pub runtime: RuntimeFileConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ScannerFileConfig {
    pub threads: Option<usize>,
    pub event_buffer: Option<usize>,
    pub apparent_size: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFileConfig {
    pub reduced_motion: Option<bool>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvironmentOverrides {
    pub root: Option<PathBuf>,
    pub scan_threads: Option<usize>,
    pub event_buffer: Option<usize>,
    pub apparent_size: Option<bool>,
    pub reduced_motion: Option<bool>,
    pub monochrome: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RuntimeConfig {
    pub root: PathBuf,
    pub scan_threads: usize,
    pub event_buffer: usize,
    pub apparent_size: bool,
    pub reduced_motion: bool,
    pub monochrome: bool,
    pub disable_delete_confirmation: bool,
    pub config_path: Option<PathBuf>,
}

impl RuntimeConfig {
    /// # Errors
    /// Returns an invalid-configuration or filesystem error when a selected layer cannot load.
    pub fn load(cli: Cli) -> Result<Self, AppError> {
        let environment = EnvironmentOverrides::from_process()?;
        let explicit_config = cli.config.clone().or_else(|| {
            env::var_os("EXCISE_CONFIG")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
        });
        let default_config = default_config_path();
        let selected_config = explicit_config
            .clone()
            .or_else(|| default_config.filter(|path| path.is_file()));
        let file = if let Some(path) = selected_config.as_ref() {
            Some(load_file(path)?)
        } else {
            None
        };
        let cwd = env::current_dir()
            .map_err(|error| AppError::io("could not determine current directory", error))?;
        Self::from_layers(cli, file.as_ref(), environment, cwd, selected_config)
    }

    /// Resolves already-parsed layers without reading process-global state.
    ///
    /// # Errors
    /// Returns an invalid-configuration error for unsupported versions or invalid bounds.
    pub fn from_layers(
        cli: Cli,
        file: Option<&FileConfig>,
        environment: EnvironmentOverrides,
        cwd: PathBuf,
        config_path: Option<PathBuf>,
    ) -> Result<Self, AppError> {
        if let Some(file) = file
            && file.version != CONFIG_VERSION
        {
            return Err(AppError::Config(format!(
                "unsupported config version {}; expected {CONFIG_VERSION}",
                file.version
            )));
        }

        let default_threads = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(8);
        let file_scanner = file.map(|file| &file.scanner);
        let file_runtime = file.map(|file| &file.runtime);
        let scan_threads = cli
            .scan_threads
            .or(environment.scan_threads)
            .or_else(|| file_scanner.and_then(|scanner| scanner.threads))
            .unwrap_or(default_threads);
        let event_buffer = cli
            .event_buffer
            .or(environment.event_buffer)
            .or_else(|| file_scanner.and_then(|scanner| scanner.event_buffer))
            .unwrap_or(DEFAULT_EVENT_BUFFER);
        validate_range("scanner threads", scan_threads, 1, MAX_SCANNER_THREADS)?;
        validate_range("event buffer", event_buffer, 16, 4096)?;

        let apparent_size = cli.apparent_size
            || environment
                .apparent_size
                .or_else(|| file_scanner.and_then(|scanner| scanner.apparent_size))
                .unwrap_or(false);
        let reduced_motion = cli.reduced_motion
            || environment
                .reduced_motion
                .or_else(|| file_runtime.and_then(|runtime| runtime.reduced_motion))
                .unwrap_or(false);
        let root = cli.folder.or(environment.root).unwrap_or(cwd);

        Ok(Self {
            root,
            scan_threads,
            event_buffer,
            apparent_size,
            reduced_motion,
            monochrome: environment.monochrome,
            disable_delete_confirmation: cli.disable_delete_confirmation,
            config_path,
        })
    }
}

impl EnvironmentOverrides {
    fn from_process() -> Result<Self, AppError> {
        Ok(Self {
            root: env::var_os("EXCISE_ROOT")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            scan_threads: parse_usize_env("EXCISE_SCAN_THREADS")?,
            event_buffer: parse_usize_env("EXCISE_EVENT_BUFFER")?,
            apparent_size: parse_bool_env("EXCISE_APPARENT_SIZE")?,
            reduced_motion: parse_bool_env("EXCISE_REDUCED_MOTION")?,
            monochrome: env::var_os("NO_COLOR").is_some(),
        })
    }
}

/// # Errors
/// Returns an invalid-configuration error for malformed or unknown TOML fields.
pub fn parse_file_config(input: &str) -> Result<FileConfig, AppError> {
    toml::from_str(input).map_err(|error| AppError::Config(error.to_string()))
}

#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "findyourexit", "excise")
        .map(|dirs| dirs.config_dir().join("config.toml"))
}

fn load_file(path: &Path) -> Result<FileConfig, AppError> {
    fs::read_to_string(path)
        .map_err(|error| {
            AppError::Config(format!(
                "could not read {}: {error}",
                path.to_string_lossy()
            ))
        })
        .and_then(|input| parse_file_config(&input))
}

fn parse_usize_env(name: &'static str) -> Result<Option<usize>, AppError> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(|value| parse_usize(name, &value))
        .transpose()
}

fn parse_usize(name: &'static str, value: &OsStr) -> Result<usize, AppError> {
    value
        .to_string_lossy()
        .parse()
        .map_err(|_| AppError::Config(format!("{name} must be an unsigned integer")))
}

fn parse_bool_env(name: &'static str) -> Result<Option<bool>, AppError> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(|value| parse_bool(name, &value.to_string_lossy()))
        .transpose()
}

fn parse_bool(name: &'static str, value: &str) -> Result<bool, AppError> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(AppError::Config(format!(
            "{name} must be true/false, yes/no, on/off, or 1/0"
        ))),
    }
}

fn validate_range(
    name: &str,
    value: usize,
    minimum: usize,
    maximum: usize,
) -> Result<(), AppError> {
    if (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(AppError::Config(format!(
            "{name} must be between {minimum} and {maximum}; got {value}"
        )))
    }
}
