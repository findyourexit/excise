use std::fs;
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use serde_json::Value;

const NATIVE_PATH_SCHEMA_ID: &str =
    "https://github.com/findyourexit/excise/schemas/native-path-v1.json";

fn scan_report_validator() -> jsonschema::Validator {
    let native_path_schema: Value =
        serde_json::from_str(include_str!("../docs/schemas/native-path.schema.json"))
            .expect("native-path schema should be valid JSON");
    let scan_report_schema: Value =
        serde_json::from_str(include_str!("../docs/schemas/scan-report.schema.json"))
            .expect("scan-report schema should be valid JSON");
    let registry = jsonschema::Registry::new()
        .add(NATIVE_PATH_SCHEMA_ID, native_path_schema)
        .expect("native-path schema should have a valid identity")
        .prepare()
        .expect("native-path registry should prepare");
    jsonschema::draft202012::options()
        .with_registry(&registry)
        .build(&scan_report_schema)
        .expect("scan-report schema should compile")
}

#[test]
fn version_output_matches_the_package_contract() -> Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg("--version")
        .output()?;
    ensure!(output.status.success(), "--version should succeed");
    let stdout = String::from_utf8(output.stdout)?;
    assert_eq!(stdout.trim(), concat!("excise ", env!("CARGO_PKG_VERSION")));
    Ok(())
}

#[test]
fn invalid_configuration_and_cli_inputs_keep_documented_exit_classes() -> Result<()> {
    let root = tempfile::tempdir()?;
    let cases = [
        (
            "unsupported.toml",
            "version = 2\n",
            78,
            "unsupported config version 2; expected 1",
        ),
        (
            "unknown.toml",
            "version = 1\nunknown = true\n",
            78,
            "unknown field",
        ),
    ];
    for (filename, contents, expected_exit, expected_message) in cases {
        let config = root.path().join(filename);
        fs::write(&config, contents)?;
        let output = Command::new(env!("CARGO_BIN_EXE_excise"))
            .args([
                "--config",
                config.to_str().context("config path should be UTF-8")?,
            ])
            .arg("--format")
            .arg("json")
            .arg(root.path())
            .output()?;
        ensure!(
            output.status.code() == Some(expected_exit),
            "{filename} should exit {expected_exit}, got {}",
            output.status
        );
        let stderr = String::from_utf8(output.stderr)?;
        assert!(
            stderr.contains(expected_message),
            "{filename} error should contain {expected_message:?}: {stderr:?}"
        );
        assert!(
            output.stdout.is_empty(),
            "invalid configuration must not emit JSON"
        );
    }

    let output = Command::new(env!("CARGO_BIN_EXE_excise"))
        .arg("--not-a-real-option")
        .output()?;
    assert_eq!(output.status.code(), Some(64));
    Ok(())
}

#[test]
fn headless_json_output_satisfies_the_published_schema() -> Result<()> {
    let root = tempfile::tempdir()?;
    fs::write(root.path().join("entry"), b"payload")?;
    let output = Command::new(env!("CARGO_BIN_EXE_excise"))
        .args(["--format", "json"])
        .arg(root.path())
        .output()?;
    ensure!(
        output.status.success(),
        "JSON scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(document["document_kind"], "scan-report");
    assert_eq!(document["schema_version"], 1);
    scan_report_validator()
        .validate(&document)
        .map_err(|error| anyhow::anyhow!("CLI JSON report violated its schema: {error}"))?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn table_output_escapes_hostile_names_without_emitting_controls() -> Result<()> {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt as _;

    let root = tempfile::tempdir()?;
    let hostile_name = OsString::from_vec(b"hostile-\n-\x1b[31m".to_vec());
    fs::write(root.path().join(hostile_name), b"payload")?;
    let output = Command::new(env!("CARGO_BIN_EXE_excise"))
        .args(["--format", "table"])
        .arg(root.path())
        .output()?;
    ensure!(
        output.status.success(),
        "table scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let table = String::from_utf8(output.stdout)?;
    assert!(table.contains("[deceptive]"));
    assert!(table.contains("\\n"));
    assert!(table.contains("\\x1b"));
    assert!(!table.contains('\u{1b}'));
    Ok(())
}
