use std::path::PathBuf;

use clap::Parser;

use crate::config::{
    Cli, EnvironmentOverrides, FileConfig, RuntimeConfig, RuntimeFileConfig, ScannerFileConfig,
    parse_file_config,
};
use crate::error::AppError;

fn cli(arguments: &[&str]) -> Cli {
    Cli::try_parse_from(arguments).expect("test CLI should parse")
}

#[test]
fn config_rejects_unknown_fields_and_versions() {
    let error = parse_file_config("version = 1\nunknown = true")
        .expect_err("unknown config field should fail");
    assert!(matches!(error, AppError::Config(_)));

    let error = RuntimeConfig::from_layers(
        cli(&["excise"]),
        Some(&FileConfig {
            version: 99,
            ..FileConfig::default()
        }),
        EnvironmentOverrides::default(),
        PathBuf::from("cwd"),
        None,
    )
    .expect_err("unsupported config version should fail");
    assert!(matches!(error, AppError::Config(_)));
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
            "cli-root",
        ]),
        Some(&FileConfig {
            version: 1,
            scanner: ScannerFileConfig {
                threads: Some(2),
                event_buffer: Some(32),
                apparent_size: Some(false),
            },
            runtime: RuntimeFileConfig {
                reduced_motion: Some(false),
            },
        }),
        EnvironmentOverrides {
            root: Some(PathBuf::from("env-root")),
            scan_threads: Some(3),
            event_buffer: Some(64),
            apparent_size: Some(false),
            reduced_motion: Some(false),
            monochrome: true,
        },
        PathBuf::from("cwd"),
        Some(PathBuf::from("config.toml")),
    )
    .expect("layered config should resolve");

    assert_eq!(config.root, PathBuf::from("cli-root"));
    assert_eq!(config.scan_threads, 4);
    assert_eq!(config.event_buffer, 64);
    assert!(config.apparent_size);
    assert!(config.reduced_motion);
    assert!(config.monochrome);
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
            },
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
