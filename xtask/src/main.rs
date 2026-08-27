use clap_complete::Shell;
use excise::cli_command;
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
const HOMEBREW_TEMPLATE: &str = "packaging/homebrew/Formula/excise.rb.in";

#[derive(Clone, Copy)]
struct UnixReleaseAsset {
    target: &'static str,
    url_placeholder: &'static str,
    hash_placeholder: &'static str,
    hash_env: &'static str,
}

const UNIX_RELEASE_ASSETS: [UnixReleaseAsset; 4] = [
    UnixReleaseAsset {
        target: "x86_64-unknown-linux-gnu",
        url_placeholder: "@X86_64_UNKNOWN_LINUX_GNU_URL@",
        hash_placeholder: "@X86_64_UNKNOWN_LINUX_GNU_SHA256@",
        hash_env: "EXCISE_SHA256_X86_64_UNKNOWN_LINUX_GNU",
    },
    UnixReleaseAsset {
        target: "aarch64-unknown-linux-gnu",
        url_placeholder: "@AARCH64_UNKNOWN_LINUX_GNU_URL@",
        hash_placeholder: "@AARCH64_UNKNOWN_LINUX_GNU_SHA256@",
        hash_env: "EXCISE_SHA256_AARCH64_UNKNOWN_LINUX_GNU",
    },
    UnixReleaseAsset {
        target: "x86_64-apple-darwin",
        url_placeholder: "@X86_64_APPLE_DARWIN_URL@",
        hash_placeholder: "@X86_64_APPLE_DARWIN_SHA256@",
        hash_env: "EXCISE_SHA256_X86_64_APPLE_DARWIN",
    },
    UnixReleaseAsset {
        target: "aarch64-apple-darwin",
        url_placeholder: "@AARCH64_APPLE_DARWIN_URL@",
        hash_placeholder: "@AARCH64_APPLE_DARWIN_SHA256@",
        hash_env: "EXCISE_SHA256_AARCH64_APPLE_DARWIN",
    },
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

const NATIVE_SUPPORT_TARGETS: [&str; 3] = [
    "x86_64-unknown-linux-gnu",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
];
const BUILD_ONLY_TARGETS: [&str; 3] = [
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "aarch64-pc-windows-msvc",
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
        Some("check-support-matrix") => check_support_matrix(),
        Some("render-homebrew") => render_homebrew_formula(),
        Some("dist-local") => build_local_dist(),
        Some("demo") => render_demo(),
        _ => Err(io::Error::other(
            "usage: cargo xtask <verify|generate|check-generated|check-distribution|check-support-matrix|render-homebrew|dist-local|demo>",
        )
        .into()),
    }
}

fn release_version() -> Result<String, Box<dyn Error>> {
    let command = cli_command();
    let version = command
        .get_version()
        .ok_or_else(|| io::Error::other("CLI version was absent"))?;
    Ok(version.to_owned())
}

fn check_support_matrix() -> Result<(), Box<dyn Error>> {
    let ci = fs::read_to_string(".github/workflows/ci.yml")?;
    let release = fs::read_to_string(".github/workflows/release.yml")?;
    let development = fs::read_to_string("docs/development.md")?;
    let support = fs::read_to_string("SUPPORT.md")?;

    for target in NATIVE_SUPPORT_TARGETS {
        require_text(&ci, target, "native CI target")?;
        require_row(
            &development,
            target,
            "Supported in `1.0.0`",
            "docs/development.md",
        )?;
        require_row(&support, target, "| Supported |", "SUPPORT.md")?;
    }
    for target in BUILD_ONLY_TARGETS {
        if ci.contains(target) {
            return Err(io::Error::other(format!(
                "build-only target {target} must not appear in the native CI matrix"
            ))
            .into());
        }
        require_text(&release, target, "release archive target")?;
        require_row(
            &development,
            target,
            "Build-only/best-effort",
            "docs/development.md",
        )?;
        require_row(&support, target, "Build-only/best-effort", "SUPPORT.md")?;
    }
    require_text(
        &development,
        "Filesystem-provider-specific",
        "filesystem caveat in docs/development.md",
    )?;
    require_text(
        &support,
        "Filesystem-provider-specific",
        "filesystem caveat in SUPPORT.md",
    )?;

    for asset in UNIX_RELEASE_ASSETS {
        require_text(&release, asset.target, "Unix release target")?;
    }
    for asset in WINDOWS_RELEASE_ASSETS {
        require_text(&release, asset.target, "Windows release target")?;
    }
    println!(
        "Support matrix validated: {} native targets, {} build-only targets.",
        NATIVE_SUPPORT_TARGETS.len(),
        BUILD_ONLY_TARGETS.len()
    );
    Ok(())
}

fn require_text(contents: &str, needle: &str, description: &str) -> Result<(), Box<dyn Error>> {
    if contents.contains(needle) {
        Ok(())
    } else {
        Err(io::Error::other(format!("missing {description}: {needle}")).into())
    }
}

fn require_row(
    contents: &str,
    target: &str,
    marker: &str,
    source: &str,
) -> Result<(), Box<dyn Error>> {
    if contents
        .lines()
        .any(|line| line.contains(target) && line.contains(marker))
    {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "{source} has no support row for {target} marked {marker}"
        ))
        .into())
    }
}

/// Frames per second the published recording keeps.
///
/// VHS captures faster than a README needs. Twenty is the floor that still
/// carries a 160 ms map tween: below it the encoder saves nothing, because each
/// surviving frame simply differs from its predecessor by more.
const DEMO_FRAMERATE: u32 = 20;
/// Palette size for the published recording.
///
/// The interface paints flat colour, so a small palette costs nothing visible
/// and every colour dropped is bytes saved on every frame.
const DEMO_COLORS: u32 = 64;
/// Lossy quantisation budget for `gifsicle`.
///
/// Measured against the same recording, this trades no observable fidelity on
/// flat surfaces and does not make idle frames shimmer.
const DEMO_LOSSY: u32 = 50;
/// Ceiling the published recording must stay under.
///
/// A README hero is fetched by everyone who visits the page, so weight is a
/// user-visible property. The optimised pipeline lands near 1.1 MiB; this
/// catches a regression that silently restores dithering or full frame rate,
/// which alone would triple the asset.
const DEMO_MAX_BYTES: u64 = 1_572_864;
/// Minimum duration retained by the tape's final asserted states.
const DEMO_MIN_DURATION_SECONDS: f64 = 10.0;
/// Minimum frames retained after VHS's frame deduplication.
const DEMO_MIN_FRAMES: u64 = 160;

/// The current `main` README hero.
///
/// `assets/demo.gif` intentionally remains the published `0.1.2` recording,
/// because the release README references that path through the moving `main`
/// branch URL.
const DEMO_CURRENT_MAIN_GIF: &str = "assets/demo-main.gif";
/// Staging output from `vhs`.
///
/// The tape names the current-main GIF so it can also be rendered directly,
/// but the task overrides that destination so a failed recording cannot reach
/// the published current-main asset.
const DEMO_RENDERED: &str = "assets/demo-main.rendered.gif";
/// Intermediate for the palette pass.
///
/// The extension is load-bearing: `ffmpeg` picks its muxer from it.
const DEMO_INTERMEDIATE: &str = "assets/demo-main.palette.gif";
/// Staging output from `gifsicle`.
///
/// Quantisation must finish and satisfy the download budget before this file
/// replaces the published current-main GIF.
const DEMO_QUANTISED: &str = "assets/demo-main.quantised.gif";
const DEMO_TAPE: &str = "tapes/demo.tape";

/// Renders the README hero recording and shrinks it to publishable weight.
///
/// `vhs` alone writes a dithered, full-rate GIF several times larger than the
/// same frames need. Dithering is the dominant cost — it turns flat terminal
/// cells into per-pixel noise that no frame differ can compress — so the
/// optimisation pass rebuilds the palette without it before quantising.
fn render_demo() -> Result<(), Box<dyn Error>> {
    let _cleanup = DemoArtifactCleanup::install();
    DemoArtifactCleanup::clear_stale()?;

    run(OsStr::new("vhs"), "validate tape", &["validate", DEMO_TAPE])?;
    run_vhs("render tape", &["--output", DEMO_RENDERED, DEMO_TAPE])?;

    let rendered = created_recording_len(DemoArtifactCleanup::rendered(), "render tape")?;

    let filter = format!(
        "fps={DEMO_FRAMERATE},split[a][b];[a]palettegen=max_colors={DEMO_COLORS}[p];[b][p]paletteuse=dither=none"
    );
    run(
        OsStr::new("ffmpeg"),
        "reduce palette",
        &[
            "-v",
            "error",
            "-y",
            "-i",
            DEMO_RENDERED,
            "-filter_complex",
            &filter,
            DEMO_INTERMEDIATE,
        ],
    )?;

    let lossy = format!("--lossy={DEMO_LOSSY}");
    run(
        OsStr::new("gifsicle"),
        "quantise frames",
        &["-O3", &lossy, DEMO_INTERMEDIATE, "-o", DEMO_QUANTISED],
    )?;

    let published = created_output_len(DemoArtifactCleanup::quantised(), "quantise frames")?;
    if published > DEMO_MAX_BYTES {
        return Err(io::Error::other(format!(
            "{DEMO_CURRENT_MAIN_GIF} is {published} bytes, above the {DEMO_MAX_BYTES} byte ceiling"
        ))
        .into());
    }

    DemoArtifactCleanup::clear_transients()?;
    fs::rename(DemoArtifactCleanup::quantised(), DEMO_CURRENT_MAIN_GIF)?;
    println!("\n{DEMO_CURRENT_MAIN_GIF}: {rendered} bytes rendered, {published} bytes published");
    Ok(())
}

/// Removes disposable demo artifacts whenever the render pipeline exits.
///
/// The checked-in GIF stays outside this guard so failures leave the last
/// published recording untouched.
struct DemoArtifactCleanup;

impl DemoArtifactCleanup {
    fn install() -> Self {
        Self
    }

    fn rendered() -> &'static Path {
        Path::new(DEMO_RENDERED)
    }

    fn intermediate() -> &'static Path {
        Path::new(DEMO_INTERMEDIATE)
    }

    fn quantised() -> &'static Path {
        Path::new(DEMO_QUANTISED)
    }

    fn clear_stale() -> io::Result<()> {
        Self::clear_all()
    }

    fn clear_transients() -> io::Result<()> {
        remove_demo_files(&[Self::rendered(), Self::intermediate()])
    }

    fn clear_all() -> io::Result<()> {
        remove_demo_files(&[Self::rendered(), Self::intermediate(), Self::quantised()])
    }
}

impl Drop for DemoArtifactCleanup {
    fn drop(&mut self) {
        if let Err(error) = Self::clear_all() {
            eprintln!("could not clean demo staging artifacts: {error}");
        }
    }
}

fn remove_demo_files(paths: &[&Path]) -> io::Result<()> {
    let mut first_error: Option<io::Error> = None;

    for &path in paths {
        if let Err(error) = remove_file_if_exists(path) {
            if first_error.is_none() {
                first_error = Some(error);
            }
        }
    }

    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

fn remove_file_if_exists(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("could not remove {}: {error}", path.display()),
        )),
    }
}

/// Verifies that a stage created a usable output after its staging path was cleared.
///
/// VHS can report a successful command after its encoder has failed, so a
/// successful exit status alone does not prove that a GIF is available.
fn created_output_len(path: &Path, stage: &str) -> io::Result<u64> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("{stage} did not create {}: {error}", path.display()),
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(io::Error::other(format!(
            "{stage} created {} but it is not a regular file",
            path.display()
        )));
    }

    let len = metadata.len();
    if len == 0 {
        return Err(io::Error::other(format!(
            "{stage} created an empty {}",
            path.display()
        )));
    }
    Ok(len)
}
/// Verifies that the staged GIF contains the complete demo rather than a
/// non-empty prefix left behind by a failed VHS encoder.
fn created_recording_len(path: &Path, stage: &str) -> io::Result<u64> {
    let len = created_output_len(path, stage)?;
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=duration,nb_read_frames",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .map_err(|error| command_start_error(stage, OsStr::new("ffprobe"), &error))?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::other(format!(
            "{stage} could not inspect {} with ffprobe: {detail}",
            path.display()
        )));
    }
    let document: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
        io::Error::other(format!(
            "{stage} returned invalid ffprobe JSON for {}: {error}",
            path.display()
        ))
    })?;
    let stream = document
        .get("streams")
        .and_then(Value::as_array)
        .and_then(|streams| streams.first())
        .and_then(Value::as_object)
        .ok_or_else(|| {
            io::Error::other(format!(
                "{stage} produced no inspectable video stream in {}",
                path.display()
            ))
        })?;
    let duration = stream
        .get("duration")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<f64>().ok())
        .ok_or_else(|| {
            io::Error::other(format!(
                "{stage} produced no finite GIF duration in {}",
                path.display()
            ))
        })?;
    let frames = stream
        .get("nb_read_frames")
        .and_then(Value::as_str)
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            io::Error::other(format!(
                "{stage} produced no GIF frame count in {}",
                path.display()
            ))
        })?;
    validate_demo_recording_metrics(duration, frames).map_err(|error| {
        io::Error::other(format!(
            "{stage} output {} is incomplete: {error}",
            path.display()
        ))
    })?;
    Ok(len)
}

fn validate_demo_recording_metrics(duration: f64, frames: u64) -> io::Result<()> {
    if !duration.is_finite() || duration < DEMO_MIN_DURATION_SECONDS {
        return Err(io::Error::other(format!(
            "duration {duration:.3}s is below the {DEMO_MIN_DURATION_SECONDS:.3}s minimum"
        )));
    }
    if frames < DEMO_MIN_FRAMES {
        return Err(io::Error::other(format!(
            "{frames} frames are below the {DEMO_MIN_FRAMES}-frame minimum"
        )));
    }
    Ok(())
}

fn verify() -> Result<(), Box<dyn Error>> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    run_static_checks(&cargo)?;
    run_behavior_checks(&cargo)?;
    check_generated()?;
    check_distribution_contract(&release_version()?)?;
    check_support_matrix()?;
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
        "CLI contract smoke",
        &["test", "--test", "cli_contract_smoke", "--locked"],
    )?;
    run(
        cargo,
        "native path smoke",
        &["test", "--test", "native_path_smoke", "--locked"],
    )?;
    run(
        cargo,
        "package verification",
        &["package", "--package", "excise", "--locked"],
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
            "--features",
            "internal",
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
            "--features",
            "internal",
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

fn command_start_error(label: &str, program: &OsStr, error: &io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!(
            "{label} could not start `{}`: {error}",
            program.to_string_lossy()
        ),
    )
}

fn run(program: &OsStr, label: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    run_command(program, label, args, |_| {})
}

fn run_vhs(label: &str, args: &[&str]) -> Result<(), Box<dyn Error>> {
    run_command(OsStr::new("vhs"), label, args, |command| {
        command.env_remove("VHS_PUBLISH");
    })
}

fn run_command(
    program: &OsStr,
    label: &str,
    args: &[&str],
    configure: impl FnOnce(&mut Command),
) -> Result<(), Box<dyn Error>> {
    println!("\n==> {label}");
    let mut command = Command::new(program);
    command.args(args);
    configure(&mut command);
    let status = command
        .status()
        .map_err(|error| command_start_error(label, program, &error))?;
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
    let mut command = Command::new(program);
    command.args(args).env(key, value);
    if !cfg!(windows) {
        command.env("EXCISE_PTY_BUDGETS", "1");
    }
    let status = command
        .status()
        .map_err(|error| command_start_error(label, program, &error))?;
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
    clap_mangen::Man::new(cli_command()).render(&mut man)?;
    let mut man = String::from_utf8(man)?
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    man.push('\n');
    artifacts.push(GeneratedArtifact {
        path: PathBuf::from("generated/man/excise.1"),
        bytes: man.into_bytes(),
    });
    for (shell, name) in [
        (Shell::Bash, "excise.bash"),
        (Shell::Zsh, "_excise"),
        (Shell::Fish, "excise.fish"),
        (Shell::PowerShell, "_excise.ps1"),
        (Shell::Elvish, "excise.elv"),
    ] {
        let mut command = cli_command();
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
    write_local_homebrew_formula(&formula_dir.join("excise.rb"), &archive_path, &archive_hash)?;
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

fn homebrew_template_values(
    version: &str,
    hashes: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, Box<dyn Error>> {
    let mut values = BTreeMap::new();
    for asset in UNIX_RELEASE_ASSETS {
        let hash = hashes.get(asset.target).ok_or_else(|| {
            io::Error::other(format!("Homebrew SHA256 was absent for {}", asset.target))
        })?;
        values.insert(
            asset.url_placeholder.to_owned(),
            release_asset_url(asset.target, version, "tar.gz"),
        );
        values.insert(asset.hash_placeholder.to_owned(), hash.clone());
    }
    Ok(values)
}

fn fixture_homebrew_hashes() -> BTreeMap<String, String> {
    UNIX_RELEASE_ASSETS
        .into_iter()
        .map(|asset| (asset.target.to_owned(), fixture_sha256(asset.target)))
        .collect()
}

fn check_homebrew_template(version: &str) -> Result<(), Box<dyn Error>> {
    let hashes = fixture_homebrew_hashes();
    let values = homebrew_template_values(version, &hashes)?;
    let rendered = render_distribution_template(Path::new(HOMEBREW_TEMPLATE), &values)?;
    validate_homebrew_formula(&rendered, version, &hashes)
}

fn render_homebrew_formula() -> Result<(), Box<dyn Error>> {
    let version = required_env("EXCISE_RELEASE_VERSION")?;
    let output = PathBuf::from(required_env("EXCISE_HOMEBREW_FORMULA")?);
    let mut hashes = BTreeMap::new();
    for asset in UNIX_RELEASE_ASSETS {
        hashes.insert(asset.target.to_owned(), required_env(asset.hash_env)?);
    }
    let values = homebrew_template_values(&version, &hashes)?;
    let rendered = render_distribution_template(Path::new(HOMEBREW_TEMPLATE), &values)?;
    validate_homebrew_formula(&rendered, &version, &hashes)?;
    fs::write(&output, rendered)?;
    println!("rendered Homebrew formula: {}", output.display());
    Ok(())
}

fn required_env(name: &str) -> Result<String, Box<dyn Error>> {
    env::var(name).map_err(|_| {
        io::Error::other(format!("required environment variable {name} was absent")).into()
    })
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
    check_homebrew_template(version)?;
    Ok(())
}

fn validate_homebrew_formula(
    rendered: &str,
    version: &str,
    hashes: &BTreeMap<String, String>,
) -> Result<(), Box<dyn Error>> {
    for required in [
        "class Excise < Formula",
        "homepage \"https://github.com/findyourexit/excise\"",
        "on_macos do",
        "on_linux do",
        "Hardware::CPU.arm?",
        "def install",
        "bin.install \"excise\"",
        "test do",
    ] {
        require(
            rendered.contains(required),
            format!("Homebrew formula omitted required declaration {required}"),
        )?;
    }
    require(
        !rendered
            .lines()
            .any(|line| line.trim_start().starts_with("version \"")),
        "Homebrew formula must derive the release version from its URL",
    )?;
    require(
        rendered
            .lines()
            .filter(|line| line.trim_start().starts_with("url \""))
            .count()
            == UNIX_RELEASE_ASSETS.len(),
        "Homebrew formula must declare one URL per Unix release target",
    )?;
    require(
        rendered
            .lines()
            .filter(|line| line.trim_start().starts_with("sha256 \""))
            .count()
            == UNIX_RELEASE_ASSETS.len(),
        "Homebrew formula must declare one SHA256 per Unix release target",
    )?;
    for asset in UNIX_RELEASE_ASSETS {
        let hash = hashes.get(asset.target).ok_or_else(|| {
            io::Error::other(format!("Homebrew SHA256 was absent for {}", asset.target))
        })?;
        require(
            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()),
            format!(
                "Homebrew SHA256 is not a hexadecimal digest for {}",
                asset.target
            ),
        )?;
        let expected_url = release_asset_url(asset.target, version, "tar.gz");
        let expected_pair = format!("url \"{expected_url}\"\n      sha256 \"{hash}\"");
        require(
            rendered.matches(&expected_pair).count() == 1,
            format!(
                "Homebrew formula does not pair the URL and SHA256 for {}",
                asset.target
            ),
        )?;
    }
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
    for asset in UNIX_RELEASE_ASSETS {
        require(
            render_binstall_template(&package_url, asset.target, version, "")?
                == release_asset_url(asset.target, version, "tar.gz"),
            format!(
                "cargo-binstall Unix URL does not resolve for {}",
                asset.target
            ),
        )?;
        require(
            render_binstall_template(&bin_dir, asset.target, version, "")?
                == format!("{}/excise", release_asset_root(asset.target, version)),
            format!(
                "cargo-binstall Unix binary path does not resolve for {}",
                asset.target
            ),
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

    #[test]
    fn demo_recording_metrics_reject_truncated_captures() {
        assert!(validate_demo_recording_metrics(10.4, 177).is_ok());
        assert!(validate_demo_recording_metrics(9.9, 177).is_err());
        assert!(validate_demo_recording_metrics(10.4, 159).is_err());
    }
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
    fn homebrew_template_resolves_to_unix_release_assets() {
        let hashes = fixture_homebrew_hashes();
        let values = match homebrew_template_values(TEST_VERSION, &hashes) {
            Ok(values) => values,
            Err(error) => panic!("Homebrew values did not build: {error}"),
        };
        let formula = match render_template(
            include_str!("../../packaging/homebrew/Formula/excise.rb.in"),
            &values,
        ) {
            Ok(formula) => formula,
            Err(error) => panic!("Homebrew template did not render: {error}"),
        };
        if let Err(error) = validate_homebrew_formula(&formula, TEST_VERSION, &hashes) {
            panic!("Homebrew template failed semantic validation: {error}");
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
