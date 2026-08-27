#[cfg(target_os = "linux")]
#[test]
fn headless_json_round_trips_invalid_utf8_filename() -> anyhow::Result<()> {
    use base64::Engine as _;
    use std::os::unix::ffi::OsStringExt as _;
    use std::process::Command;

    let root = tempfile::tempdir()?;
    let name = std::ffi::OsString::from_vec(b"invalid-\xff-name".to_vec());
    let path = root.path().join(&name);
    std::fs::write(&path, b"payload")?;

    let output = Command::new(env!("CARGO_BIN_EXE_excise"))
        .args(["--format", "json"])
        .arg(root.path())
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "headless scan failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let document: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    anyhow::ensure!(document["state"] == "exact", "scan should be exact");
    let entries = document["entries"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("report entries should be an array"))?;
    let entry = entries
        .iter()
        .find(|entry| {
            entry["display_path"]
                .as_str()
                .is_some_and(|display| display.contains("invalid-\\xff-name"))
        })
        .ok_or_else(|| anyhow::anyhow!("invalid-byte entry should be reported"))?;
    let encoded = entry["path"]["data"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("native path data should be a string"))?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(encoded)?;
    anyhow::ensure!(
        decoded.as_slice() == path.as_os_str().as_bytes(),
        "native path should round-trip exactly"
    );
    anyhow::ensure!(
        entry["path"]["encoding"] == "unix-bytes",
        "invalid-byte path should use unix-bytes encoding"
    );
    anyhow::ensure!(
        entry["display_path"]
            .as_str()
            .is_some_and(|display| display.contains("\\xff")),
        "invalid-byte path should use an escaped display"
    );
    Ok(())
}
