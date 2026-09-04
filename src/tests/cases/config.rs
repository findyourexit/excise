use std::path::PathBuf;

use clap::Parser;

use crate::config::{
    Cli, CustomKeyBindings, EnvironmentOverrides, FileConfig, KeyPreset, ModelFileConfig,
    NORMAL_MODE_RESERVED_CUSTOM_MOVEMENT_KEYS, RuntimeConfig, RuntimeFileConfig, SafePreferences,
    ScannerFileConfig, parse_file_config, save_safe_preferences,
};
use crate::error::AppError;
use crate::theme::ThemeId;

fn cli(arguments: &[&str]) -> Cli {
    Cli::try_parse_from(arguments).expect("test CLI should parse")
}

#[test]
fn config_rejects_unknown_fields_and_versions() {
    let error = parse_file_config("version = 1\nunknown = true")
        .expect_err("unknown config field should fail");
    assert!(matches!(error, AppError::Config(_)));

    for version in [0, 2, u16::MAX] {
        let error = RuntimeConfig::from_layers(
            cli(&["excise"]),
            Some(&FileConfig {
                version,
                ..FileConfig::default()
            }),
            EnvironmentOverrides::default(),
            PathBuf::from("cwd"),
            None,
        )
        .expect_err("unsupported config version should fail");
        assert!(matches!(&error, AppError::Config(_)));
        assert!(
            error
                .to_string()
                .contains(&format!("unsupported config version {version}; expected 1")),
            "version {version} should explain the stable configuration boundary"
        );
    }
}
#[test]
fn malformed_scanner_exclusion_is_a_configuration_error() {
    let error = RuntimeConfig::from_layers(
        cli(&["excise", "--exclude", "[z-a]"]),
        None,
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect_err("malformed scanner exclusions should fail configuration resolution");

    assert!(
        matches!(error, AppError::Config(message) if message.contains("invalid scanner exclusion"))
    );
}

#[test]
fn temporary_storage_limit_must_be_at_least_two_mib() {
    let error = RuntimeConfig::from_layers(
        cli(&["excise", "--temporary-storage-mib", "1"]),
        None,
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect_err("one MiB temporary storage should fail configuration resolution");

    assert!(matches!(error, AppError::Config(_)));
    assert!(
        error
            .to_string()
            .contains("temporary storage must be between 2")
    );
}

#[test]
fn precedence_is_cli_then_environment_then_file_then_default() {
    let config = RuntimeConfig::from_layers(
        cli(&[
            "excise",
            "--scan-threads",
            "4",
            "--apparent-size",
            "--reduced-motion",
            "--theme",
            "dracula",
            "--ascii",
            "--mouse",
            "--keymap",
            "emacs",
            "--temporary-storage-mib",
            "4",
            "cli-root",
        ]),
        Some(&FileConfig {
            version: 1,
            scanner: ScannerFileConfig {
                threads: Some(2),
                event_buffer: Some(32),
                apparent_size: Some(false),
                ..ScannerFileConfig::default()
            },
            runtime: RuntimeFileConfig {
                reduced_motion: Some(false),
                ..RuntimeFileConfig::default()
            },
            model: ModelFileConfig {
                process_memory_mib: Some(crate::model::DEFAULT_PROCESS_MIB),
                temporary_storage_mib: Some(2),
            },
        }),
        EnvironmentOverrides {
            root: Some(PathBuf::from("env-root")),
            scan_threads: Some(3),
            event_buffer: Some(64),
            apparent_size: Some(false),
            reduced_motion: Some(false),
            monochrome: true,
            cross_filesystems: Some(false),
            exclusions: Vec::new(),
            memory_mib: Some(crate::model::DEFAULT_PROCESS_MIB),
            temporary_storage_mib: Some(3),
            theme: None,
            ascii: None,
            mouse: None,
            keymap: None,
            format: None,
            output: None,
        },
        PathBuf::from("cwd"),
        Some(PathBuf::from("config.toml")),
    )
    .expect("layered config should resolve");

    assert_eq!(config.root, PathBuf::from("cli-root"));
    assert_eq!(config.scan_threads, 4);
    assert_eq!(config.event_buffer, 64);
    assert_eq!(config.temporary_storage_mib, 4);
    assert!(config.apparent_size);
    assert!(config.reduced_motion);
    assert!(config.monochrome);
    assert_eq!(config.theme, ThemeId::Dracula);
    assert!(config.ascii);
    assert!(config.mouse);
    assert_eq!(config.keymap, KeyPreset::Emacs);
}

#[test]
fn explicit_false_environment_value_overrides_file_true() {
    let config = RuntimeConfig::from_layers(
        cli(&["excise"]),
        Some(&FileConfig {
            version: 1,
            scanner: ScannerFileConfig {
                apparent_size: Some(true),
                ..ScannerFileConfig::default()
            },
            runtime: RuntimeFileConfig {
                reduced_motion: Some(true),
                ..RuntimeFileConfig::default()
            },
            model: ModelFileConfig::default(),
        }),
        EnvironmentOverrides {
            apparent_size: Some(false),
            reduced_motion: Some(false),
            ..EnvironmentOverrides::default()
        },
        PathBuf::from("cwd"),
        None,
    )
    .expect("environment overrides should resolve");

    assert!(!config.apparent_size);
    assert!(!config.reduced_motion);
}

#[test]
fn saved_preferences_preserve_scanner_config_and_exclude_delete_guardrails() {
    let directory = tempfile::tempdir().expect("config directory should exist");
    let path = directory.path().join("config.toml");
    std::fs::write(
        &path,
        "version = 1\n[scanner]\nthreads = 3\n[runtime]\nreduced_motion = false\n",
    )
    .expect("initial config should be written");

    save_safe_preferences(
        &path,
        SafePreferences {
            theme: ThemeId::Nord,
            ascii: true,
            mouse: true,
            keymap: KeyPreset::Emacs,
            custom_keys: None,
            reduced_motion: true,
        },
    )
    .expect("safe preferences should save");

    let saved = std::fs::read_to_string(&path).expect("saved config should be readable");
    let parsed = parse_file_config(&saved).expect("saved config should parse");
    assert_eq!(parsed.scanner.threads, Some(3));
    assert_eq!(parsed.runtime.theme, Some(ThemeId::Nord));
    assert_eq!(parsed.runtime.ascii, Some(true));
    assert_eq!(parsed.runtime.mouse, Some(true));
    assert_eq!(parsed.runtime.keymap, Some(KeyPreset::Emacs));
    assert_eq!(parsed.runtime.reduced_motion, Some(true));
    assert!(!saved.contains("delete"));
}

#[test]
fn safe_preferences_validate_custom_keymaps_before_writing() {
    let directory = tempfile::tempdir().expect("config directory should exist");
    let path = directory.path().join("config.toml");

    let error = save_safe_preferences(
        &path,
        SafePreferences {
            theme: ThemeId::Nord,
            ascii: true,
            mouse: true,
            keymap: KeyPreset::Custom,
            custom_keys: None,
            reduced_motion: true,
        },
    )
    .expect_err("custom preferences without bindings should not save");
    assert!(matches!(error, AppError::Config(_)));
    assert!(error.to_string().contains("requires [runtime.custom_keys]"));
    assert!(!path.exists());

    let error = save_safe_preferences(
        &path,
        SafePreferences {
            theme: ThemeId::Nord,
            ascii: true,
            mouse: true,
            keymap: KeyPreset::Vim,
            custom_keys: Some(CustomKeyBindings {
                left: 'e',
                down: 's',
                up: 'w',
                right: 'd',
            }),
            reduced_motion: true,
        },
    )
    .expect_err("invalid inactive custom bindings should not save");
    assert!(matches!(error, AppError::Config(_)));
    assert!(error.to_string().contains("export"));
    assert!(!path.exists());
}

#[test]
#[allow(clippy::too_many_lines)]
fn custom_keymap_requires_distinct_reachable_bindings() {
    let mut file = FileConfig {
        version: 1,
        runtime: RuntimeFileConfig {
            keymap: Some(KeyPreset::Custom),
            custom_keys: Some(CustomKeyBindings {
                left: 'a',
                down: 's',
                up: 'w',
                right: 'd',
            }),
            ..RuntimeFileConfig::default()
        },
        ..FileConfig::default()
    };
    let config = RuntimeConfig::from_layers(
        cli(&["excise"]),
        Some(&file),
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect("safe custom bindings should resolve");
    assert_eq!(config.keymap, KeyPreset::Custom);

    file.runtime.custom_keys = Some(CustomKeyBindings {
        left: 'x',
        down: 'x',
        up: 'w',
        right: 'd',
    });
    let error = RuntimeConfig::from_layers(
        cli(&["excise"]),
        Some(&file),
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect_err("duplicate bindings should fail");
    assert!(matches!(error, AppError::Config(_)));
    assert!(error.to_string().contains("distinct"));

    let reserved = [
        ('/', "filter"),
        ('?', "help"),
        ('e', "export"),
        ('t', "theme"),
        ('q', "quit"),
        ('+', "zoom in"),
        ('-', "zoom out"),
        ('0', "reset zoom"),
    ];
    assert_eq!(
        NORMAL_MODE_RESERVED_CUSTOM_MOVEMENT_KEYS,
        reserved.as_slice()
    );
    for (key, action) in reserved {
        file.runtime.custom_keys = Some(CustomKeyBindings {
            left: key,
            down: 's',
            up: 'w',
            right: 'd',
        });
        let error = RuntimeConfig::from_layers(
            cli(&["excise"]),
            Some(&file),
            EnvironmentOverrides::default(),
            PathBuf::from("cwd"),
            None,
        )
        .expect_err("normal-mode commands cannot become movement bindings");
        assert!(matches!(error, AppError::Config(_)));
        assert!(
            error.to_string().contains(action),
            "{key:?} should name the {action} command"
        );
    }

    for key in ['A', '!', '\n', '\u{8}'] {
        file.runtime.custom_keys = Some(CustomKeyBindings {
            left: key,
            down: 's',
            up: 'w',
            right: 'd',
        });
        let error = RuntimeConfig::from_layers(
            cli(&["excise"]),
            Some(&file),
            EnvironmentOverrides::default(),
            PathBuf::from("cwd"),
            None,
        )
        .expect_err("modified or control binding should fail");
        assert!(matches!(error, AppError::Config(_)));
        assert!(
            error.to_string().contains("unmodified printable ASCII"),
            "{key:?} should explain the supported key shape"
        );
    }

    file.runtime.keymap = Some(KeyPreset::Vim);
    file.runtime.custom_keys = Some(CustomKeyBindings {
        left: 'e',
        down: 's',
        up: 'w',
        right: 'd',
    });
    let error = RuntimeConfig::from_layers(
        cli(&["excise"]),
        Some(&file),
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect_err("invalid inactive custom bindings should fail");
    assert!(matches!(error, AppError::Config(_)));
    assert!(error.to_string().contains("export"));

    file.runtime.keymap = Some(KeyPreset::Custom);
    file.runtime.custom_keys = None;
    let error = RuntimeConfig::from_layers(
        cli(&["excise"]),
        Some(&file),
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect_err("custom keymap without bindings should fail");
    assert!(matches!(error, AppError::Config(_)));
    assert!(error.to_string().contains("requires [runtime.custom_keys]"));
}

#[test]
fn output_file_requires_noninteractive_format() {
    let error = RuntimeConfig::from_layers(
        cli(&["excise", "--output", "report.json"]),
        None,
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect_err("TUI output path should be rejected");
    assert!(matches!(error, AppError::Config(_)));

    let config = RuntimeConfig::from_layers(
        cli(&["excise", "--format", "json", "--output", "report.json"]),
        None,
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect("JSON output path should resolve");
    assert_eq!(config.format, crate::config::OutputFormat::Json);
    assert_eq!(config.output, Some(PathBuf::from("report.json")));
}
