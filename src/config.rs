use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use clap::{Parser, ValueEnum};
use directories::ProjectDirs;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::native_path::{safe_display_path_text, safe_display_text};
use crate::theme::ThemeId;

fn config_error(message: impl std::fmt::Display) -> AppError {
    let message = message.to_string();
    AppError::Config(safe_display_text(&message))
}

pub const CONFIG_VERSION: u16 = 1;
const DEFAULT_EVENT_BUFFER: usize = 256;
const MAX_SCANNER_THREADS: usize = 32;
const DEFAULT_MAX_SCANNER_THREADS: usize = 8;

fn default_scan_threads(available: usize) -> usize {
    available
        .saturating_sub(1)
        .clamp(1, DEFAULT_MAX_SCANNER_THREADS)
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum KeyPreset {
    #[default]
    Vim,
    Custom,
    Emacs,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[value(rename_all = "kebab-case")]
pub enum OutputFormat {
    #[default]
    Tui,
    Table,
    Json,
}

/// Normal-mode commands that must remain available when custom movement is active.
pub(crate) const NORMAL_MODE_RESERVED_CUSTOM_MOVEMENT_KEYS: &[(char, &str)] = &[
    ('/', "filter"),
    ('?', "help"),
    ('e', "export"),
    ('t', "theme"),
    ('q', "quit"),
    ('+', "zoom in"),
    ('-', "zoom out"),
    ('0', "reset zoom"),
];

#[must_use]
pub(crate) fn normal_mode_action_for_custom_key(key: char) -> Option<&'static str> {
    NORMAL_MODE_RESERVED_CUSTOM_MOVEMENT_KEYS
        .iter()
        .find_map(|(reserved_key, action)| (*reserved_key == key).then_some(*action))
}

#[must_use]
pub(crate) fn is_supported_custom_movement_key(key: char) -> bool {
    is_unmodified_printable_custom_movement_key(key)
        && normal_mode_action_for_custom_key(key).is_none()
}

const fn is_unmodified_printable_custom_movement_key(key: char) -> bool {
    matches!(
        key,
        ' '
            | '0'..='9'
            | 'a'..='z'
            | '\''
            | ','
            | '-'
            | '.'
            | '/'
            | ';'
            | '='
            | '['
            | '\\'
            | ']'
            | '`'
    )
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustomKeyBindings {
    pub left: char,
    pub down: char,
    pub up: char,
    pub right: char,
}

impl CustomKeyBindings {
    fn validate(&self) -> Result<(), AppError> {
        let bindings = [
            ("left", self.left),
            ("down", self.down),
            ("up", self.up),
            ("right", self.right),
        ];
        for (direction, key) in bindings {
            if let Some(action) = normal_mode_action_for_custom_key(key) {
                return Err(config_error(format!(
                    "custom movement key {direction} ({key:?}) conflicts with the normal-mode {action} command; choose another key"
                )));
            }
            if !is_supported_custom_movement_key(key) {
                let reason = if key.is_uppercase() {
                    "uses Shift"
                } else {
                    "is not an unmodified printable ASCII key"
                };
                return Err(config_error(format!(
                    "custom movement key {direction} ({key:?}) {reason}; choose an unmodified printable ASCII key"
                )));
            }
        }
        for (index, (direction, key)) in bindings.iter().enumerate() {
            if let Some((other_direction, _)) = bindings[..index]
                .iter()
                .find(|(_, existing_key)| existing_key == key)
            {
                return Err(config_error(format!(
                    "custom movement keys {other_direction} and {direction} both use {key:?}; use four distinct keys"
                )));
            }
        }
        Ok(())
    }
}

fn validate_custom_keymap(
    keymap: KeyPreset,
    custom_keys: Option<&CustomKeyBindings>,
) -> Result<(), AppError> {
    if let Some(bindings) = custom_keys {
        bindings.validate()?;
    }
    if keymap == KeyPreset::Custom && custom_keys.is_none() {
        return Err(AppError::Config(
            "keymap custom requires [runtime.custom_keys]".to_string(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Parser)]
#[allow(clippy::struct_excessive_bools)]
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
    /// Permit traversal across filesystem boundaries
    pub cross_filesystems: bool,
    #[arg(long = "exclude", value_name = "PATTERN", action = clap::ArgAction::Append)]
    /// Ordered gitignore-style exclusion pattern
    pub exclusions: Vec<String>,
    #[arg(long, value_name = "MIB")]
    /// Whole-process memory envelope in MiB
    pub memory_mib: Option<usize>,
    #[arg(long)]
    /// Disable nonessential motion
    pub reduced_motion: bool,
    #[arg(long, value_enum, value_name = "THEME")]
    /// Built-in semantic color theme
    pub theme: Option<ThemeId>,
    #[arg(long)]
    /// Use ASCII-only symbols and borders
    pub ascii: bool,
    #[arg(long)]
    /// Enable mouse capture and selection
    pub mouse: bool,
    #[arg(long, value_enum, value_name = "PRESET")]
    /// Keyboard preset. Arrows and safety keys always work.
    pub keymap: Option<KeyPreset>,
    #[arg(long, value_enum)]
    /// Output mode. Table and JSON never acquire a terminal.
    pub format: Option<OutputFormat>,
    #[arg(long, value_name = "FILE")]
    /// Write a noninteractive report to FILE instead of stdout
    pub output: Option<PathBuf>,
    #[arg(short, long)]
    /// Do not ask for confirmation before deleting
    pub disable_delete_confirmation: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub version: u16,
    #[serde(default)]
    pub scanner: ScannerFileConfig,
    #[serde(default)]
    pub runtime: RuntimeFileConfig,
    #[serde(default)]
    pub model: ModelFileConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ScannerFileConfig {
    pub threads: Option<usize>,
    pub event_buffer: Option<usize>,
    pub apparent_size: Option<bool>,
    pub cross_filesystems: Option<bool>,
    #[serde(default)]
    pub exclusions: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelFileConfig {
    pub process_memory_mib: Option<usize>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeFileConfig {
    pub reduced_motion: Option<bool>,
    pub theme: Option<ThemeId>,
    pub ascii: Option<bool>,
    pub mouse: Option<bool>,
    pub keymap: Option<KeyPreset>,
    pub format: Option<OutputFormat>,
    pub custom_keys: Option<CustomKeyBindings>,
    pub output: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EnvironmentOverrides {
    pub root: Option<PathBuf>,
    pub scan_threads: Option<usize>,
    pub event_buffer: Option<usize>,
    pub apparent_size: Option<bool>,
    pub reduced_motion: Option<bool>,
    pub cross_filesystems: Option<bool>,
    pub exclusions: Vec<String>,
    pub memory_mib: Option<usize>,
    pub monochrome: bool,
    pub theme: Option<ThemeId>,
    pub ascii: Option<bool>,
    pub mouse: Option<bool>,
    pub keymap: Option<KeyPreset>,
    pub format: Option<OutputFormat>,
    pub output: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(clippy::struct_excessive_bools)]
pub struct RuntimeConfig {
    pub root: PathBuf,
    pub scan_threads: usize,
    pub event_buffer: usize,
    pub cross_filesystems: bool,
    pub exclusions: Vec<String>,
    pub memory_mib: usize,
    pub apparent_size: bool,
    pub reduced_motion: bool,
    pub monochrome: bool,
    pub disable_delete_confirmation: bool,
    pub theme: ThemeId,
    pub ascii: bool,
    pub mouse: bool,
    pub keymap: KeyPreset,
    pub format: OutputFormat,
    pub output: Option<PathBuf>,
    pub custom_keys: Option<CustomKeyBindings>,
    pub config_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SafePreferences {
    pub theme: ThemeId,
    pub ascii: bool,
    pub mouse: bool,
    pub keymap: KeyPreset,
    pub custom_keys: Option<CustomKeyBindings>,
    pub reduced_motion: bool,
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
    #[allow(clippy::too_many_lines)]
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

        let default_threads =
            default_scan_threads(std::thread::available_parallelism().map_or(1, usize::from));
        let file_scanner = file.map(|file| &file.scanner);
        let file_runtime = file.map(|file| &file.runtime);
        let file_model = file.map(|file| &file.model);
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
        let maximum_memory_mib =
            crate::model::detected_memory_limit_mib().max(crate::model::MIN_PROCESS_MIB);
        let memory_mib = cli
            .memory_mib
            .or(environment.memory_mib)
            .or_else(|| file_model.and_then(|model| model.process_memory_mib))
            .unwrap_or(crate::model::DEFAULT_PROCESS_MIB.min(maximum_memory_mib));
        validate_range("scanner threads", scan_threads, 1, MAX_SCANNER_THREADS)?;
        validate_range("event buffer", event_buffer, 16, 4096)?;
        validate_range(
            "process memory",
            memory_mib,
            crate::model::MIN_PROCESS_MIB,
            maximum_memory_mib,
        )?;

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
        let theme = cli
            .theme
            .or(environment.theme)
            .or_else(|| file_runtime.and_then(|runtime| runtime.theme))
            .unwrap_or_default();
        let ascii = cli.ascii
            || environment
                .ascii
                .or_else(|| file_runtime.and_then(|runtime| runtime.ascii))
                .unwrap_or(false);
        let mouse = cli.mouse
            || environment
                .mouse
                .or_else(|| file_runtime.and_then(|runtime| runtime.mouse))
                .unwrap_or(false);
        let keymap = cli
            .keymap
            .or(environment.keymap)
            .or_else(|| file_runtime.and_then(|runtime| runtime.keymap))
            .unwrap_or_default();
        let custom_keys = file_runtime.and_then(|runtime| runtime.custom_keys.clone());
        validate_custom_keymap(keymap, custom_keys.as_ref())?;
        let format = cli
            .format
            .or(environment.format)
            .or_else(|| file_runtime.and_then(|runtime| runtime.format))
            .unwrap_or_default();
        let output = cli
            .output
            .or(environment.output)
            .or_else(|| file_runtime.and_then(|runtime| runtime.output.clone()));
        if format == OutputFormat::Tui && output.is_some() {
            return Err(AppError::Config(
                "--output requires --format table or --format json".to_string(),
            ));
        }
        let cross_filesystems = cli.cross_filesystems
            || environment
                .cross_filesystems
                .or_else(|| file_scanner.and_then(|scanner| scanner.cross_filesystems))
                .unwrap_or(false);
        let exclusions = if !cli.exclusions.is_empty() {
            cli.exclusions
        } else if !environment.exclusions.is_empty() {
            environment.exclusions
        } else {
            file_scanner.map_or_else(Vec::new, |scanner| scanner.exclusions.clone())
        };
        let root = cli.folder.or(environment.root).unwrap_or(cwd);
        compile_exclusions(&root, &exclusions)?;

        Ok(Self {
            root,
            scan_threads,
            event_buffer,
            memory_mib,
            apparent_size,
            cross_filesystems,
            exclusions,
            custom_keys,
            reduced_motion,
            monochrome: environment.monochrome || theme == ThemeId::Monochrome,
            theme,
            ascii,
            mouse,
            keymap,
            disable_delete_confirmation: cli.disable_delete_confirmation,
            config_path,
            format,
            output,
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
            theme: parse_value_enum_env("EXCISE_THEME")?,
            ascii: parse_bool_env("EXCISE_ASCII")?,
            mouse: parse_bool_env("EXCISE_MOUSE")?,
            keymap: parse_value_enum_env("EXCISE_KEYMAP")?,
            format: parse_value_enum_env("EXCISE_FORMAT")?,
            output: env::var_os("EXCISE_OUTPUT")
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            cross_filesystems: parse_bool_env("EXCISE_CROSS_FILESYSTEMS")?,
            exclusions: env::var("EXCISE_EXCLUDE")
                .ok()
                .map(|value| {
                    value
                        .split(';')
                        .filter(|item| !item.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            memory_mib: parse_usize_env("EXCISE_MEMORY_MIB")?,
        })
    }
}

/// Atomically persists only non-destructive UI preferences.
///
/// # Errors
/// Returns a configuration or filesystem error without weakening deletion guardrails.
pub fn save_safe_preferences(path: &Path, preferences: SafePreferences) -> Result<(), AppError> {
    validate_custom_keymap(preferences.keymap, preferences.custom_keys.as_ref())?;
    let mut config = if path.is_file() {
        load_file(path)?
    } else {
        FileConfig::default()
    };
    config.version = CONFIG_VERSION;
    config.runtime.theme = Some(preferences.theme);
    config.runtime.ascii = Some(preferences.ascii);
    config.runtime.mouse = Some(preferences.mouse);
    config.runtime.keymap = Some(preferences.keymap);
    config.runtime.reduced_motion = Some(preferences.reduced_motion);
    config.runtime.custom_keys = preferences.custom_keys;
    let serialized = toml::to_string_pretty(&config)
        .map_err(|error| config_error(format!("could not serialize config: {error}")))?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| AppError::io("could not create config directory", error))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| AppError::io("could not create temporary config", error))?;
    temporary
        .write_all(serialized.as_bytes())
        .and_then(|()| temporary.flush())
        .map_err(|error| AppError::io("could not write temporary config", error))?;
    temporary
        .persist(path)
        .map_err(|error| AppError::io("could not replace config", error.error))?;
    Ok(())
}

/// # Errors
/// Returns an invalid-configuration error for malformed or unknown TOML fields.
pub fn parse_file_config(input: &str) -> Result<FileConfig, AppError> {
    toml::from_str(input).map_err(|error| config_error(error.to_string()))
}

#[must_use]
pub fn default_config_path() -> Option<PathBuf> {
    ProjectDirs::from("dev", "findyourexit", "excise")
        .map(|dirs| dirs.config_dir().join("config.toml"))
}

pub(crate) fn compile_exclusions(root: &Path, patterns: &[String]) -> Result<Gitignore, AppError> {
    let mut builder = GitignoreBuilder::new(root);
    for pattern in patterns {
        builder.add_line(None, pattern).map_err(|error| {
            config_error(format!("invalid scanner exclusion {pattern:?}: {error}"))
        })?;
    }
    builder
        .build()
        .map_err(|error| config_error(format!("invalid scanner exclusions: {error}")))
}

fn load_file(path: &Path) -> Result<FileConfig, AppError> {
    fs::read_to_string(path)
        .map_err(|error| {
            config_error(format!(
                "could not read {}: {error}",
                safe_display_path_text(path)
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
fn parse_value_enum_env<T>(name: &'static str) -> Result<Option<T>, AppError>
where
    T: ValueEnum + Clone,
{
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(|value| {
            T::from_str(&value.to_string_lossy(), true)
                .map_err(|error| config_error(format!("{name}: {error}")))
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_scan_threads_leaves_a_cpu_for_interaction() {
        assert_eq!(default_scan_threads(1), 1);
        assert_eq!(default_scan_threads(2), 1);
        assert_eq!(default_scan_threads(8), 7);
        assert_eq!(default_scan_threads(9), 8);
        assert_eq!(default_scan_threads(64), 8);
    }

    #[test]
    fn configuration_errors_escape_terminal_controls() {
        let error = config_error("bad\n\u{202e}name\u{1b}[31m");
        let message = error.to_string();
        assert!(message.starts_with("invalid configuration: [deceptive]"));
        assert!(message.contains("\\n"));
        assert!(message.contains("\\u{202e}"));
        assert!(message.contains("\\x1b"));
        assert!(!message.chars().any(char::is_control));
        assert!(!message.contains('\u{202e}'));
    }

    #[cfg(unix)]
    #[test]
    fn missing_configuration_path_keeps_invalid_bytes_reversible() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let path = PathBuf::from(OsString::from_vec(b"missing-\xff/config.toml".to_vec()));
        let error = load_file(&path).expect_err("missing configuration should fail");
        let message = error.to_string();
        assert!(message.contains("[deceptive]"));
        assert!(message.contains("missing-\\xff/config.toml"));
    }
}
