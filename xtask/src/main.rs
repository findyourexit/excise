use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::Path;
use std::process::Command;

const EXCEPTION_FILE: &str = "verification/exceptions.txt";

fn main() {
    if let Err(error) = dispatch() {
        eprintln!("verification failed: {error}");
        std::process::exit(1);
    }
}

fn dispatch() -> Result<(), Box<dyn Error>> {
    if env::args().nth(1).as_deref() != Some("verify") {
        return Err(io::Error::other("usage: cargo verify").into());
    }

    ensure_exception_ratchet_is_empty()?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    run_static_checks(&cargo)?;
    run_behavior_checks(&cargo)?;
    run_dynamic_checks(&cargo)?;

    println!("\nLocal verification passed with zero exceptions.");
    Ok(())
}

fn run_static_checks(cargo: &OsStr) -> Result<(), Box<dyn Error>> {
    run(cargo, "format", &["fmt", "--all", "--", "--check"])?;
    run(OsStr::new("actionlint"), "workflow syntax", &[])?;
    let npx = if cfg!(windows) { "npx.cmd" } else { "npx" };
    run(
        OsStr::new(npx),
        "Renovate syntax",
        &[
            "--yes",
            "--package",
            "renovate@44.34.0",
            "renovate-config-validator",
            ".github/renovate.json5",
        ],
    )?;
    run(
        OsStr::new("lychee"),
        "documentation links",
        &[
            "--offline",
            "--no-progress",
            "README.md",
            "CONTRIBUTING.md",
            "GOVERNANCE.md",
            "MAINTAINERS.md",
            "SECURITY.md",
            "docs",
        ],
    )?;
    run(
        cargo,
        "compile",
        &["check", "--workspace", "--all-targets", "--locked"],
    )?;
    check_installed_cross_targets(cargo)?;
    run(
        cargo,
        "strict Clippy",
        &[
            "clippy",
            "--workspace",
            "--all-targets",
            "--locked",
            "--",
            "-D",
            "warnings",
        ],
    )
}

fn run_behavior_checks(cargo: &OsStr) -> Result<(), Box<dyn Error>> {
    run(
        cargo,
        "unit and snapshot tests",
        &["test", "--workspace", "--lib", "--bins", "--locked"],
    )?;
    run(
        cargo,
        "release PTY binary",
        &["build", "--release", "--package", "excise", "--locked"],
    )?;
    let pty_binary = fs::canonicalize(if cfg!(windows) {
        "target/release/excise.exe"
    } else {
        "target/release/excise"
    })?;
    run_with_env(
        cargo,
        "pseudo-terminal smoke and budgets",
        &["test", "--test", "pty_smoke", "--locked"],
        "EXCISE_PTY_BINARY",
        pty_binary.as_os_str(),
    )?;
    run(
        cargo,
        "package verification",
        &[
            "package",
            "--package",
            "excise",
            "--locked",
            "--allow-dirty",
        ],
    )?;
    run(cargo, "dependency policy", &["deny", "check"])
}

fn run_dynamic_checks(cargo: &OsStr) -> Result<(), Box<dyn Error>> {
    run_fuzz_target("truncate", 512)?;
    run_fuzz_target("config", 512)?;
    run_fuzz_target("native_path", 512)?;
    run_fuzz_target("terminal_state", 512)?;
    run_fuzz_target("tachyonfx", 512)?;
    run_fuzz_target("runtime_events", 64)?;
    run(
        cargo,
        "TachyonFX render benchmark",
        &[
            "bench",
            "--bench",
            "tachyonfx",
            "--locked",
            "--",
            "--noplot",
            "--sample-size",
            "10",
            "--warm-up-time",
            "0.1",
            "--measurement-time",
            "0.2",
        ],
    )?;
    run(
        cargo,
        "release binary measurement",
        &["build", "--release", "--package", "excise", "--locked"],
    )?;
    report_release_binary_size()
}

fn run_fuzz_target(target: &str, iterations: u32) -> Result<(), Box<dyn Error>> {
    let runs = format!("-runs={iterations}");
    run(
        OsStr::new("rustup"),
        &format!("fuzz smoke: {target}"),
        &[
            "run",
            "nightly",
            "cargo",
            "fuzz",
            "run",
            target,
            "--",
            &runs,
            "-max_len=4096",
        ],
    )
}

fn ensure_exception_ratchet_is_empty() -> Result<(), Box<dyn Error>> {
    let content = fs::read_to_string(EXCEPTION_FILE)?;
    let active: Vec<_> = content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect();
    if active.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{} active verification exception(s) remain in {EXCEPTION_FILE}",
            active.len()
        ))
        .into())
    }
}

fn check_installed_cross_targets(cargo: &OsStr) -> Result<(), Box<dyn Error>> {
    let output = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("could not enumerate installed Rust targets").into());
    }
    let installed = String::from_utf8(output.stdout)?;
    for target in [
        "aarch64-apple-darwin",
        "x86_64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "x86_64-unknown-linux-gnu",
    ] {
        if installed.lines().any(|installed| installed == target) {
            run(
                cargo,
                &format!("cross-check {target}"),
                &["check", "--locked", "--bin", "excise", "--target", target],
            )?;
        }
    }
    Ok(())
}

fn run(program: &OsStr, label: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    println!("\n==> {label}");
    let status = Command::new(program).args(args).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{label} exited with {status}")).into())
    }
}

fn run_with_env(
    program: &OsStr,
    label: &str,
    args: &[&str],
    key: &str,
    value: &OsStr,
) -> Result<(), Box<dyn Error>> {
    println!("\n==> {label}");
    let status = Command::new(program).args(args).env(key, value).status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("{label} exited with {status}")).into())
    }
}

fn report_release_binary_size() -> Result<(), Box<dyn Error>> {
    let binary = if cfg!(windows) {
        Path::new("target/release/excise.exe")
    } else {
        Path::new("target/release/excise")
    };
    let bytes = fs::metadata(binary)?.len();
    let mebibytes = bytes / 1_048_576;
    let hundredths = (u128::from(bytes % 1_048_576) * 100) / 1_048_576;
    println!("release binary: {bytes} bytes ({mebibytes}.{hundredths:02} MiB)");
    Ok(())
}
