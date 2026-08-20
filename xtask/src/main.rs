use clap::CommandFactory as _;
use clap_complete::Shell;
use excise::config::Cli;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

const NATIVE_PATH_SCHEMA_ID: &str =
    "https://github.com/findyourexit/excise/schemas/native-path-v1.json";
const RELEASE_REPOSITORY: &str = "https://github.com/findyourexit/excise";
const SCOOP_TEMPLATE: &str = "packaging/scoop/excise.json.in";
const WINGET_TEMPLATE: &str = "packaging/winget/FindYourExit.Excise.yaml.in";
const UNIX_RELEASE_TARGETS: [&str; 4] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
];
const WINDOWS_RELEASE_ASSETS: [WindowsReleaseAsset; 2] = [
    WindowsReleaseAsset {
        target: "x86_64-pc-windows-msvc",
        scoop_architecture: "64bit",
        winget_architecture: "x64",
        url_placeholder: "@X86_64_PC_WINDOWS_MSVC_URL@",
        hash_placeholder: "@X86_64_PC_WINDOWS_MSVC_SHA256@",
    },
    WindowsReleaseAsset {
        target: "aarch64-pc-windows-msvc",
        scoop_architecture: "arm64",
        winget_architecture: "arm64",
        url_placeholder: "@AARCH64_PC_WINDOWS_MSVC_URL@",
        hash_placeholder: "@AARCH64_PC_WINDOWS_MSVC_SHA256@",
    },
];

fn main() {
    if let Err(error) = dispatch() {
        eprintln!("verification failed: {error}");
        std::process::exit(1);
    }
}

fn dispatch() -> Result<(), Box<dyn Error>> {
    match env::args().nth(1).as_deref() {
        Some("verify") => verify(),
        Some("generate") => write_generated(),
        Some("check-generated") => check_generated(),
        Some("check-distribution") => check_distribution_contract(&release_version()?),
        Some("dist-local") => build_local_dist(),
        _ => Err(io::Error::other(
            "usage: cargo xtask <verify|generate|check-generated|check-distribution|dist-local>",
        )
        .into()),
    }
}

fn release_version() -> Result<String, Box<dyn Error>> {
    let command = Cli::command();
    let version = command
        .get_version()
        .ok_or_else(|| io::Error::other("CLI version was absent"))?;
    Ok(version.to_owned())
}

fn verify() -> Result<(), Box<dyn Error>> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    run_static_checks(&cargo)?;
    run_behavior_checks(&cargo)?;
    check_generated()?;
    check_distribution_contract(&release_version()?)?;
    run_dynamic_checks(&cargo)?;

    println!("\nLocal verification passed.");
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
            "SUPPORT.md",
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
    run_fuzz_target("treemap", 512)?;
    run_fuzz_target("accounting", 256)?;
    run_fuzz_target("filter", 512)?;
    run_fuzz_target("deletion_state", 256)?;
    run_fuzz_target("deletion_plan", 64)?;
    run_fuzz_target("report", 512)?;
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
        "100k treemap benchmark",
        &[
            "bench",
            "--bench",
            "core",
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
    let corpus = format!("fuzz/corpus/{target}");
    let seed = format!("fuzz/seeds/{target}");
    fs::create_dir_all(&corpus)?;
    let mut args = vec!["run", "nightly", "cargo", "fuzz", "run", target, &corpus];
    if Path::new(&seed).is_dir() {
        args.push(&seed);
    }
    args.extend(["--", &runs, "-max_len=4096"]);
    run(
        OsStr::new("rustup"),
        &format!("fuzz smoke: {target}"),
        &args,
    )
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
struct GeneratedArtifact {
    path: PathBuf,
    bytes: Vec<u8>,
}

fn generated_artifacts() -> Result<Vec<GeneratedArtifact>, Box<dyn Error>> {
    let mut artifacts = Vec::new();
    let mut man = Vec::new();
    clap_mangen::Man::new(Cli::command()).render(&mut man)?;
    artifacts.push(GeneratedArtifact {
        path: PathBuf::from("generated/man/excise.1"),
        bytes: man,
    });
    for (shell, name) in [
        (Shell::Bash, "excise.bash"),
        (Shell::Zsh, "_excise"),
        (Shell::Fish, "excise.fish"),
        (Shell::PowerShell, "_excise.ps1"),
        (Shell::Elvish, "excise.elv"),
    ] {
        let mut command = Cli::command();
        let mut bytes = Vec::new();
        clap_complete::generate(shell, &mut command, "excise", &mut bytes);
        artifacts.push(GeneratedArtifact {
            path: PathBuf::from("generated/completions").join(name),
            bytes,
        });
    }
    Ok(artifacts)
}

fn write_generated() -> Result<(), Box<dyn Error>> {
    for artifact in generated_artifacts()? {
        if let Some(parent) = artifact.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&artifact.path, artifact.bytes)?;
        println!("generated {}", artifact.path.display());
    }
    Ok(())
}

fn check_generated() -> Result<(), Box<dyn Error>> {
    for artifact in generated_artifacts()? {
        let actual = fs::read(&artifact.path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "generated artifact {} is missing: {error}; run cargo generate",
                    artifact.path.display()
                ),
            )
        })?;
        if actual != artifact.bytes {
            return Err(io::Error::other(format!(
                "generated artifact {} drifted; run cargo generate",
                artifact.path.display()
            ))
            .into());
        }
    }
    validate_published_schemas()?;
    Ok(())
}

fn validate_published_schemas() -> Result<(), Box<dyn Error>> {
    let native_path = read_schema("docs/schemas/native-path.schema.json")?;
    let scan_report = read_schema("docs/schemas/scan-report.schema.json")?;
    let deletion_history = read_schema("docs/schemas/deletion-history.schema.json")?;
    if native_path.get("$id").and_then(Value::as_str) != Some(NATIVE_PATH_SCHEMA_ID) {
        return Err(io::Error::other(format!(
            "native-path schema must retain canonical id {NATIVE_PATH_SCHEMA_ID}"
        ))
        .into());
    }
    for (path, schema) in [
        ("docs/schemas/native-path.schema.json", &native_path),
        ("docs/schemas/scan-report.schema.json", &scan_report),
        (
            "docs/schemas/deletion-history.schema.json",
            &deletion_history,
        ),
    ] {
        if schema.get("$schema").and_then(Value::as_str)
            != Some("https://json-schema.org/draft/2020-12/schema")
        {
            return Err(io::Error::other(format!(
                "published schema {path} must declare Draft 2020-12"
            ))
            .into());
        }
        jsonschema::draft202012::meta::validate(schema).map_err(|error| {
            io::Error::other(format!(
                "published schema {path} is not Draft 2020-12: {error}"
            ))
        })?;
    }

    jsonschema::draft202012::options()
        .build(&native_path)
        .map_err(|error| {
            io::Error::other(format!(
                "published schema docs/schemas/native-path.schema.json does not compile: {error}"
            ))
        })?;
    let registry = jsonschema::Registry::new()
        .add(NATIVE_PATH_SCHEMA_ID, native_path.clone())
        .map_err(|error| {
            io::Error::other(format!("could not register native-path schema: {error}"))
        })?
        .prepare()
        .map_err(|error| {
            io::Error::other(format!("could not prepare native-path schema: {error}"))
        })?;
    for (path, schema) in [
        ("docs/schemas/scan-report.schema.json", &scan_report),
        (
            "docs/schemas/deletion-history.schema.json",
            &deletion_history,
        ),
    ] {
        jsonschema::draft202012::options()
            .with_registry(&registry)
            .build(schema)
            .map_err(|error| {
                io::Error::other(format!("published schema {path} does not compile: {error}"))
            })?;
    }
    Ok(())
}

fn read_schema(path: &str) -> Result<Value, Box<dyn Error>> {
    serde_json::from_slice(&fs::read(path)?).map_err(|error| {
        io::Error::other(format!(
            "published schema {path} is not valid JSON: {error}"
        ))
        .into()
    })
}

fn build_local_dist() -> Result<(), Box<dyn Error>> {
    check_generated()?;
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    run(
        &cargo,
        "release binary",
        &["build", "--release", "--package", "excise", "--locked"],
    )?;
    let rustc = env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
    let output = Command::new(rustc).arg("-vV").output()?;
    if !output.status.success() {
        return Err(io::Error::other("rustc -vV failed").into());
    }
    let verbose = String::from_utf8(output.stdout)?;
    let host = verbose
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .ok_or_else(|| io::Error::other("rustc host triple was absent"))?;
    let version = release_version()?;
    let archive_name = format!("excise-{host}-v{version}");
    let dist = PathBuf::from("dist");
    fs::create_dir_all(&dist)?;
    let staging = dist.join(&archive_name);
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let binary_name = if cfg!(windows) {
        "excise.exe"
    } else {
        "excise"
    };
    fs::copy(
        Path::new("target/release").join(binary_name),
        staging.join(binary_name),
    )?;
    for file in ["LICENSE", "README.md"] {
        fs::copy(file, staging.join(file))?;
    }
    copy_tree(Path::new("generated"), &staging.join("generated"))?;
    copy_tree(Path::new("docs/schemas"), &staging.join("schemas"))?;
    write_sbom(&staging.join("excise.cdx.json"), &version)?;
    validate_cyclonedx_1_5_bom(&staging.join("excise.cdx.json"))?;
    check_distribution_contract(&version)?;
    write_provenance(
        &staging.join("provenance.local.json"),
        host,
        &version,
        &staging.join(binary_name),
    )?;

    let archive_path = dist.join(format!("{archive_name}.tar.gz"));
    let archive_file = fs::File::create(&archive_path)?;
    let encoder = GzEncoder::new(archive_file, Compression::best());
    let mut archive = tar::Builder::new(encoder);
    archive.append_dir_all(&archive_name, &staging)?;
    let encoder = archive.into_inner()?;
    encoder.finish()?;

    let archive_hash = sha256_file(&archive_path)?;
    let sbom_hash = sha256_file(&staging.join("excise.cdx.json"))?;
    fs::write(
        dist.join("checksums.sha256"),
        format!(
            "{archive_hash}  {}\n{sbom_hash}  {archive_name}/excise.cdx.json\n",
            archive_path
                .file_name()
                .and_then(OsStr::to_str)
                .ok_or_else(|| io::Error::other("archive name was not UTF-8"))?
        ),
    )?;
    let formula_dir = dist.join("homebrew");
    fs::create_dir_all(&formula_dir)?;
    write_local_homebrew_formula(
        &formula_dir.join("excise.rb"),
        &archive_path,
        &archive_hash,
        &version,
    )?;
    verify_archive(&archive_path, &archive_name, binary_name)?;
    println!("local release archive: {}", archive_path.display());
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<(), Box<dyn Error>> {
    fs::create_dir_all(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

fn write_local_homebrew_formula(
    path: &Path,
    archive: &Path,
    hash: &str,
    version: &str,
) -> Result<(), Box<dyn Error>> {
    let archive = fs::canonicalize(archive)?;
    let formula = format!(
        r##"# typed: strict
# frozen_string_literal: true

# Local release-equivalent formula generated by `cargo dist-local`.
class Excise < Formula
  desc "Surgical terminal storage navigator"
  homepage "https://github.com/findyourexit/excise"
  url "file://{url}"
  version "{version}"
  sha256 "{hash}"
  license "MIT"

  def install
    bin.install "excise"
    man1.install "generated/man/excise.1"
    bash_completion.install "generated/completions/excise.bash" => "excise"
    zsh_completion.install "generated/completions/_excise"
    fish_completion.install "generated/completions/excise.fish"
    pkgshare.install "schemas"
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/excise --version")
    assert_match "scan-report", shell_output("#{{bin}}/excise --format json #{{testpath}}")
  end
end
"##,
        url = archive.display(),
    );
    fs::write(path, formula)?;
    Ok(())
}

fn write_sbom(path: &Path, version: &str) -> Result<(), Box<dyn Error>> {
    let metadata = cargo_metadata::MetadataCommand::new().exec()?;
    let components = metadata
        .packages
        .iter()
        .map(|package| {
            json!({
                "type": "library",
                "name": package.name.as_str(),
                "version": package.version.to_string(),
                "purl": format!("pkg:cargo/{}@{}", package.name, package.version),
                "licenses": package.license.as_ref().map(|license| vec![json!({
                    "expression": license
                })]).unwrap_or_default()
            })
        })
        .collect::<Vec<_>>();
    fs::write(
        path,
        serde_json::to_vec_pretty(&cyclonedx_1_5_document(version, &components))?,
    )?;
    Ok(())
}

fn cyclonedx_1_5_document(version: &str, components: &[Value]) -> Value {
    json!({
        "$schema": "http://cyclonedx.org/schema/bom-1.5.schema.json",
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {
            "component": { "type": "application", "name": "excise", "version": version }
        },
        "components": components
    })
}

#[derive(Clone, Copy)]
struct WindowsReleaseAsset {
    target: &'static str,
    scoop_architecture: &'static str,
    winget_architecture: &'static str,
    url_placeholder: &'static str,
    hash_placeholder: &'static str,
}

#[derive(Default)]
struct WingetInstallerManifest {
    fields: BTreeMap<String, String>,
    installers: Vec<WingetInstaller>,
}

#[derive(Default)]
struct WingetInstaller {
    fields: BTreeMap<String, String>,
    nested_installer_files: Vec<BTreeMap<String, String>>,
}

fn release_asset_root(target: &str, version: &str) -> String {
    format!("excise-{target}-v{version}")
}

fn release_asset_url(target: &str, version: &str, extension: &str) -> String {
    format!(
        "{RELEASE_REPOSITORY}/releases/download/v{version}/{}.{}",
        release_asset_root(target, version),
        extension
    )
}

fn windows_archive_executable(asset: WindowsReleaseAsset, version: &str) -> PathBuf {
    PathBuf::from(release_asset_root(asset.target, version)).join("excise.exe")
}

fn fixture_sha256(target: &str) -> String {
    format!("{:x}", Sha256::digest(target.as_bytes()))
}

fn distribution_template_values(version: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::from([("@VERSION@".to_owned(), version.to_owned())]);
    for asset in WINDOWS_RELEASE_ASSETS {
        values.insert(
            asset.url_placeholder.to_owned(),
            release_asset_url(asset.target, version, "zip"),
        );
        values.insert(
            asset.hash_placeholder.to_owned(),
            fixture_sha256(asset.target),
        );
    }
    values
}

fn render_distribution_template(
    path: &Path,
    values: &BTreeMap<String, String>,
) -> Result<String, Box<dyn Error>> {
    render_template(&fs::read_to_string(path)?, values)
}

fn render_template(
    template: &str,
    values: &BTreeMap<String, String>,
) -> Result<String, Box<dyn Error>> {
    let mut rendered = template.to_owned();
    for (placeholder, replacement) in values {
        if !rendered.contains(placeholder) {
            return Err(io::Error::other(format!(
                "distribution template omitted required placeholder {placeholder}"
            ))
            .into());
        }
        rendered = rendered.replace(placeholder, replacement);
    }
    if rendered.contains('@') {
        return Err(
            io::Error::other("distribution template contains an unresolved placeholder").into(),
        );
    }
    Ok(rendered)
}

fn check_distribution_contract(version: &str) -> Result<(), Box<dyn Error>> {
    validate_binstall_metadata(&fs::read_to_string("Cargo.toml")?, version)?;
    let values = distribution_template_values(version);
    let scoop = render_distribution_template(Path::new(SCOOP_TEMPLATE), &values)?;
    validate_scoop_manifest(&scoop, version)?;
    let winget = render_distribution_template(Path::new(WINGET_TEMPLATE), &values)?;
    validate_winget_manifest(&winget, version)?;
    Ok(())
}

fn cargo_toml_string(manifest: &str, section: &str, key: &str) -> Result<String, Box<dyn Error>> {
    let section_header = format!("[{section}]");
    let mut in_section = false;
    for raw_line in manifest.lines() {
        let line = raw_line.trim();
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line == section_header;
            continue;
        }
        if !in_section || line.starts_with('#') {
            continue;
        }
        let Some((found_key, value)) = line.split_once('=') else {
            continue;
        };
        if found_key.trim() != key {
            continue;
        }
        let value = value.trim();
        let quoted = value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .ok_or_else(|| io::Error::other(format!("{section}.{key} must be a quoted string")))?;
        return Ok(quoted.to_owned());
    }
    Err(io::Error::other(format!("Cargo.toml omitted {section}.{key}")).into())
}

fn render_binstall_template(
    template: &str,
    target: &str,
    version: &str,
    binary_extension: &str,
) -> Result<String, Box<dyn Error>> {
    let mut rendered = template.to_owned();
    for (placeholder, replacement) in [
        ("{ repo }", RELEASE_REPOSITORY),
        ("{ name }", "excise"),
        ("{ target }", target),
        ("{ version }", version),
        ("{ bin }", "excise"),
        ("{ binary-ext }", binary_extension),
    ] {
        rendered = rendered.replace(placeholder, replacement);
    }
    if rendered.contains('{') || rendered.contains('}') {
        return Err(
            io::Error::other("cargo-binstall template contains an unresolved field").into(),
        );
    }
    Ok(rendered)
}

fn validate_binstall_metadata(manifest: &str, version: &str) -> Result<(), Box<dyn Error>> {
    let base_section = "package.metadata.binstall";
    let windows_section = "package.metadata.binstall.overrides.'cfg(target_os = \"windows\")'";
    let package_url = cargo_toml_string(manifest, base_section, "pkg-url")?;
    let bin_dir = cargo_toml_string(manifest, base_section, "bin-dir")?;
    let package_format = cargo_toml_string(manifest, base_section, "pkg-fmt")?;
    let windows_url = cargo_toml_string(manifest, windows_section, "pkg-url")?;
    let windows_format = cargo_toml_string(manifest, windows_section, "pkg-fmt")?;
    require(
        package_format == "tgz",
        "cargo-binstall must extract Unix .tar.gz assets as tgz",
    )?;
    for target in UNIX_RELEASE_TARGETS {
        require(
            render_binstall_template(&package_url, target, version, "")?
                == release_asset_url(target, version, "tar.gz"),
            format!("cargo-binstall Unix URL does not resolve for {target}"),
        )?;
        require(
            render_binstall_template(&bin_dir, target, version, "")?
                == format!("{}/excise", release_asset_root(target, version)),
            format!("cargo-binstall Unix binary path does not resolve for {target}"),
        )?;
    }
    require(
        windows_format == "zip",
        "cargo-binstall must extract Windows assets as zip",
    )?;
    for asset in WINDOWS_RELEASE_ASSETS {
        require(
            render_binstall_template(&windows_url, asset.target, version, ".exe")?
                == release_asset_url(asset.target, version, "zip"),
            format!(
                "cargo-binstall Windows URL does not resolve for {}",
                asset.target
            ),
        )?;
        require(
            render_binstall_template(&bin_dir, asset.target, version, ".exe")?
                == format!("{}/excise.exe", release_asset_root(asset.target, version)),
            format!(
                "cargo-binstall Windows binary path does not resolve for {}",
                asset.target
            ),
        )?;
    }
    Ok(())
}

fn require(condition: bool, message: impl Into<String>) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(io::Error::other(message.into()).into())
    }
}

fn required_json_object<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a Map<String, Value>, Box<dyn Error>> {
    let value = object
        .get(field)
        .and_then(Value::as_object)
        .ok_or_else(|| io::Error::other(format!("{context} must contain object {field}")))?;
    Ok(value)
}

fn required_json_string<'a>(
    object: &'a Map<String, Value>,
    field: &str,
    context: &str,
) -> Result<&'a str, Box<dyn Error>> {
    let value = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other(format!("{context} must contain string {field}")))?;
    Ok(value)
}

fn resolve_scoop_autoupdate_pattern(
    pattern: &str,
    version: &str,
    field: &str,
) -> Result<String, Box<dyn Error>> {
    require(
        pattern.contains("$version"),
        format!("Scoop autoupdate {field} must retain $version"),
    )?;
    Ok(pattern.replace("$version", version))
}

fn validate_scoop_manifest(rendered: &str, version: &str) -> Result<(), Box<dyn Error>> {
    let manifest: Value = serde_json::from_str(rendered)?;
    let manifest = manifest
        .as_object()
        .ok_or_else(|| io::Error::other("Scoop manifest must be a JSON object"))?;
    require(
        required_json_string(manifest, "version", "Scoop manifest")? == version,
        "Scoop manifest must render the release version",
    )?;
    let binary = required_json_string(manifest, "bin", "Scoop manifest")?;
    let architectures = required_json_object(manifest, "architecture", "Scoop manifest")?;
    let autoupdate = required_json_object(manifest, "autoupdate", "Scoop manifest")?;
    let autoupdate_architectures =
        required_json_object(autoupdate, "architecture", "Scoop autoupdate")?;
    for asset in WINDOWS_RELEASE_ASSETS {
        let context = format!("Scoop {} architecture", asset.scoop_architecture);
        let architecture = architectures
            .get(asset.scoop_architecture)
            .and_then(Value::as_object)
            .ok_or_else(|| io::Error::other(format!("{context} is absent")))?;
        require(
            required_json_string(architecture, "url", &context)?
                == release_asset_url(asset.target, version, "zip"),
            format!("{context} URL does not resolve to its release asset"),
        )?;
        require(
            required_json_string(architecture, "hash", &context)? == fixture_sha256(asset.target),
            format!("{context} hash does not render"),
        )?;
        let extract_dir = required_json_string(architecture, "extract_dir", &context)?;
        require(
            extract_dir == release_asset_root(asset.target, version),
            format!("{context} extract_dir does not select its archive wrapper"),
        )?;
        let executable = PathBuf::from(extract_dir).join(binary);
        require(
            windows_archive_executable(asset, version) == executable,
            format!("{context} would not install the wrapped executable"),
        )?;

        let autoupdate_context = format!("Scoop {} autoupdate", asset.scoop_architecture);
        let autoupdate_architecture = autoupdate_architectures
            .get(asset.scoop_architecture)
            .and_then(Value::as_object)
            .ok_or_else(|| io::Error::other(format!("{autoupdate_context} is absent")))?;
        require(
            resolve_scoop_autoupdate_pattern(
                required_json_string(autoupdate_architecture, "url", &autoupdate_context)?,
                version,
                "URL",
            )? == release_asset_url(asset.target, version, "zip"),
            format!("{autoupdate_context} URL does not resolve to its release asset"),
        )?;
        require(
            resolve_scoop_autoupdate_pattern(
                required_json_string(autoupdate_architecture, "extract_dir", &autoupdate_context)?,
                version,
                "extract_dir",
            )? == release_asset_root(asset.target, version),
            format!("{autoupdate_context} extract_dir does not select its archive wrapper"),
        )?;
    }
    Ok(())
}

fn parse_yaml_mapping(line: &str) -> Result<(String, String), Box<dyn Error>> {
    let (key, value) = line
        .split_once(':')
        .ok_or_else(|| io::Error::other(format!("expected YAML mapping: {line}")))?;
    let key = key.trim();
    require(!key.is_empty(), "YAML mapping key must not be empty")?;
    let value = value.trim();
    let value = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .or_else(|| {
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        })
        .unwrap_or(value);
    Ok((key.to_owned(), value.to_owned()))
}

#[allow(clippy::too_many_lines)]
fn parse_winget_installer_manifest(
    rendered: &str,
) -> Result<WingetInstallerManifest, Box<dyn Error>> {
    let installer_document = rendered
        .split("\n---\n")
        .find(|document| {
            document
                .lines()
                .any(|line| line.trim() == "ManifestType: installer")
        })
        .ok_or_else(|| io::Error::other("Winget template omitted an installer document"))?;
    let mut manifest = WingetInstallerManifest::default();
    let mut installer = None;
    let mut in_installers = false;
    let mut in_nested_installer_files = false;
    for raw_line in installer_document.lines() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indentation = raw_line.len() - raw_line.trim_start_matches(' ').len();
        match indentation {
            0 => {
                if let Some(installer) = installer.take() {
                    manifest.installers.push(installer);
                }
                let (key, value) = parse_yaml_mapping(trimmed)?;
                in_installers = key == "Installers";
                if !in_installers {
                    manifest.fields.insert(key, value);
                }
                in_nested_installer_files = false;
            }
            2 if !in_installers => {}
            2 => {
                let item = trimmed
                    .strip_prefix("- ")
                    .ok_or_else(|| io::Error::other("Winget installer must be a YAML list item"))?;
                let (key, value) = parse_yaml_mapping(item)?;
                require(
                    key == "Architecture",
                    "Winget installer list item must begin with Architecture",
                )?;
                if let Some(installer) = installer.take() {
                    manifest.installers.push(installer);
                }
                let mut next = WingetInstaller::default();
                next.fields.insert(key, value);
                installer = Some(next);
                in_nested_installer_files = false;
            }
            4 => {
                let installer = installer.as_mut().ok_or_else(|| {
                    io::Error::other("Winget installer property appeared before Architecture")
                })?;
                let (key, value) = parse_yaml_mapping(trimmed)?;
                if key == "NestedInstallerFiles" {
                    require(
                        value.is_empty(),
                        "Winget NestedInstallerFiles must contain a YAML list",
                    )?;
                    in_nested_installer_files = true;
                } else {
                    installer.fields.insert(key, value);
                    in_nested_installer_files = false;
                }
            }
            6 => {
                require(
                    in_nested_installer_files,
                    "unexpected nested Winget installer field",
                )?;
                let item = trimmed.strip_prefix("- ").ok_or_else(|| {
                    io::Error::other("Winget nested installer must be a YAML list item")
                })?;
                let (key, value) = parse_yaml_mapping(item)?;
                let installer = installer.as_mut().ok_or_else(|| {
                    io::Error::other("Winget nested installer appeared before Architecture")
                })?;
                installer
                    .nested_installer_files
                    .push(BTreeMap::from([(key, value)]));
            }
            8 => {
                require(
                    in_nested_installer_files,
                    "unexpected nested Winget installer property",
                )?;
                let (key, value) = parse_yaml_mapping(trimmed)?;
                let installer = installer.as_mut().ok_or_else(|| {
                    io::Error::other("Winget nested installer appeared before Architecture")
                })?;
                let nested = installer.nested_installer_files.last_mut().ok_or_else(|| {
                    io::Error::other("Winget nested installer property lacked a list item")
                })?;
                nested.insert(key, value);
            }
            _ => {
                return Err(io::Error::other(format!(
                    "unsupported indentation in Winget installer manifest: {raw_line}"
                ))
                .into());
            }
        }
    }
    if let Some(installer) = installer {
        manifest.installers.push(installer);
    }
    Ok(manifest)
}

fn validate_winget_manifest(rendered: &str, version: &str) -> Result<(), Box<dyn Error>> {
    let manifest = parse_winget_installer_manifest(rendered)?;
    require(
        manifest
            .fields
            .get("PackageIdentifier")
            .is_some_and(|identifier| identifier == "FindYourExit.Excise"),
        "Winget installer manifest must identify Excise",
    )?;
    require(
        manifest
            .fields
            .get("PackageVersion")
            .is_some_and(|manifest_version| manifest_version == version),
        "Winget installer manifest must render the release version",
    )?;
    require(
        manifest
            .fields
            .get("InstallerType")
            .is_some_and(|installer_type| installer_type == "zip"),
        "Winget installers must declare the downloaded archive as zip",
    )?;
    require(
        manifest
            .fields
            .get("NestedInstallerType")
            .is_some_and(|installer_type| installer_type == "portable"),
        "Winget ZIP installers must declare portable nested executables",
    )?;
    require(
        manifest.installers.len() == WINDOWS_RELEASE_ASSETS.len(),
        "Winget installer manifest must declare each Windows architecture exactly once",
    )?;
    for asset in WINDOWS_RELEASE_ASSETS {
        let context = format!("Winget {} installer", asset.winget_architecture);
        let installer = manifest
            .installers
            .iter()
            .find(|installer| {
                installer
                    .fields
                    .get("Architecture")
                    .is_some_and(|architecture| architecture == asset.winget_architecture)
            })
            .ok_or_else(|| io::Error::other(format!("{context} is absent")))?;
        require(
            installer
                .fields
                .get("InstallerUrl")
                .is_some_and(|url| url == &release_asset_url(asset.target, version, "zip")),
            format!("{context} URL does not resolve to its release asset"),
        )?;
        require(
            installer
                .fields
                .get("InstallerSha256")
                .is_some_and(|hash| hash == &fixture_sha256(asset.target)),
            format!("{context} hash does not render"),
        )?;
        require(
            installer.nested_installer_files.len() == 1,
            format!("{context} must declare exactly one nested executable"),
        )?;
        let nested = installer
            .nested_installer_files
            .first()
            .ok_or_else(|| io::Error::other(format!("{context} omitted its nested executable")))?;
        let relative_path = nested
            .get("RelativeFilePath")
            .ok_or_else(|| io::Error::other(format!("{context} omitted RelativeFilePath")))?;
        let expected_relative_path =
            format!("{}/excise.exe", release_asset_root(asset.target, version));
        require(
            relative_path == &expected_relative_path,
            format!("{context} nested path does not select the archive executable"),
        )?;
        require(
            nested
                .get("PortableCommandAlias")
                .is_some_and(|alias| alias == "excise"),
            format!("{context} must publish the excise command alias"),
        )?;
        require(
            windows_archive_executable(asset, version) == PathBuf::from(relative_path),
            format!("{context} would not install the wrapped executable"),
        )?;
    }
    Ok(())
}

fn cyclonedx_1_5_output_schema() -> Value {
    json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "additionalProperties": false,
        "required": ["$schema", "bomFormat", "specVersion", "version", "metadata", "components"],
        "properties": {
            "$schema": { "const": "http://cyclonedx.org/schema/bom-1.5.schema.json" },
            "bomFormat": { "const": "CycloneDX" },
            "specVersion": { "const": "1.5" },
            "serialNumber": {
                "type": "string",
                "pattern": "^urn:uuid:[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"
            },
            "version": { "type": "integer", "minimum": 1 },
            "metadata": {
                "type": "object",
                "additionalProperties": false,
                "required": ["component"],
                "properties": { "component": { "$ref": "#/definitions/component" } }
            },
            "components": {
                "type": "array",
                "items": { "$ref": "#/definitions/component" }
            }
        },
        "definitions": {
            "component": {
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "name", "version"],
                "properties": {
                    "type": { "enum": ["application", "library"] },
                    "name": { "type": "string", "minLength": 1 },
                    "version": { "type": "string", "minLength": 1 },
                    "purl": { "type": "string", "pattern": "^pkg:cargo/" },
                    "licenses": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["expression"],
                            "properties": { "expression": { "type": "string", "minLength": 1 } }
                        }
                    }
                }
            }
        }
    })
}

fn validate_cyclonedx_1_5_bom(path: &Path) -> Result<(), Box<dyn Error>> {
    let document: Value = serde_json::from_slice(&fs::read(path)?)?;
    validate_cyclonedx_1_5_document(&document)
}

fn validate_cyclonedx_1_5_document(document: &Value) -> Result<(), Box<dyn Error>> {
    let schema = cyclonedx_1_5_output_schema();
    jsonschema::draft7::meta::validate(&schema).map_err(|error| {
        io::Error::other(format!("CycloneDX 1.5 output schema is invalid: {error}"))
    })?;
    let validator = jsonschema::draft7::options()
        .build(&schema)
        .map_err(|error| {
            io::Error::other(format!("could not build CycloneDX validator: {error}"))
        })?;
    validator
        .validate(document)
        .map_err(|error| io::Error::other(format!("invalid CycloneDX 1.5 SBOM: {error}")))?;
    Ok(())
}

fn write_provenance(
    path: &Path,
    host: &str,
    version: &str,
    binary: &Path,
) -> Result<(), Box<dyn Error>> {
    let binary_hash = sha256_file(binary)?;
    let binary_name = binary
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| io::Error::other("binary name was not UTF-8"))?;
    let document = json!({
        "_type": "https://in-toto.io/Statement/v1",
        "predicateType": "https://slsa.dev/provenance/v1",
        "subject": [{
            "name": binary_name,
            "digest": { "sha256": binary_hash }
        }],
        "predicate": {
            "buildDefinition": {
                "buildType": "https://github.com/findyourexit/excise/development-build/v1",
                "externalParameters": { "host": host, "version": version }
            },
            "runDetails": {
                "builder": { "id": "local development build; not a release attestation" }
            }
        }
    });
    fs::write(path, serde_json::to_vec_pretty(&document)?)?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, Box<dyn Error>> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024].into_boxed_slice();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verify_archive(path: &Path, root: &str, binary: &str) -> Result<(), Box<dyn Error>> {
    let file = fs::File::open(path)?;
    let decoder = GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut paths = Vec::new();
    for entry in archive.entries()? {
        paths.push(entry?.path()?.into_owned());
    }
    for required in [
        format!("{root}/{binary}"),
        format!("{root}/LICENSE"),
        format!("{root}/generated/man/excise.1"),
        format!("{root}/schemas/scan-report.schema.json"),
        format!("{root}/excise.cdx.json"),
        format!("{root}/provenance.local.json"),
    ] {
        if !paths.iter().any(|path| path == Path::new(&required)) {
            return Err(io::Error::other(format!("archive omitted {required}")).into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_VERSION: &str = "1.2.3";

    fn render_test_template(template: &str) -> String {
        match render_template(template, &distribution_template_values(TEST_VERSION)) {
            Ok(rendered) => rendered,
            Err(error) => panic!("test template did not render: {error}"),
        }
    }

    #[test]
    fn distribution_templates_resolve_to_wrapped_windows_executables() {
        let scoop = render_test_template(include_str!("../../packaging/scoop/excise.json.in"));
        if let Err(error) = validate_scoop_manifest(&scoop, TEST_VERSION) {
            panic!("Scoop template failed semantic validation: {error}");
        }
        let winget = render_test_template(include_str!(
            "../../packaging/winget/FindYourExit.Excise.yaml.in"
        ));
        if let Err(error) = validate_winget_manifest(&winget, TEST_VERSION) {
            panic!("Winget template failed semantic validation: {error}");
        }
    }

    #[test]
    fn cargo_binstall_metadata_resolves_emitted_asset_names() {
        if let Err(error) =
            validate_binstall_metadata(include_str!("../../Cargo.toml"), TEST_VERSION)
        {
            panic!("cargo-binstall metadata failed semantic validation: {error}");
        }
    }

    #[test]
    fn cyclonedx_output_has_no_invalid_serial_number() {
        let document = cyclonedx_1_5_document(
            TEST_VERSION,
            &[serde_json::json!({
                "type": "library",
                "name": "dependency",
                "version": "1.0.0",
                "purl": "pkg:cargo/dependency@1.0.0",
                "licenses": []
            })],
        );
        assert!(document.get("serialNumber").is_none());
        if let Err(error) = validate_cyclonedx_1_5_document(&document) {
            panic!("CycloneDX output failed validation: {error}");
        }
    }

    #[test]
    fn cyclonedx_validation_rejects_an_invalid_serial_number() {
        let mut document = cyclonedx_1_5_document(
            TEST_VERSION,
            &[serde_json::json!({
                "type": "library",
                "name": "dependency",
                "version": "1.0.0",
                "purl": "pkg:cargo/dependency@1.0.0",
                "licenses": []
            })],
        );
        let Some(root) = document.as_object_mut() else {
            panic!("CycloneDX document must be an object");
        };
        root.insert(
            "serialNumber".to_owned(),
            Value::String("urn:uuid:excise-1.2.3".to_owned()),
        );
        assert!(validate_cyclonedx_1_5_document(&document).is_err());
    }
}
