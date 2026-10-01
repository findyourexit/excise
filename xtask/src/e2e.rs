//! `cargo xtask e2e`: runs the pseudo-terminal scenarios against a release build of `excise`.
//!
//! This is a thin wrapper. Scenario loading, fixtures, the runner, and the run summary all live in
//! `excise-harness`; this file parses the command line, finds or builds the binary, and turns the
//! run's verdicts into an exit status.

use std::{
    env,
    error::Error,
    ffi::OsString,
    io,
    path::{Path, PathBuf},
    process::Command,
    time::Instant,
};

use excise_harness::{
    report::Tier,
    runner::{E2eOptions, load_scenarios, run_e2e},
    scenario::{Profile, Scenario},
};

const USAGE: &str = "usage: cargo xtask e2e [--quick|--full|--nightly] [--scenario NAME]... \
                     [--profile PROFILE]... [--repeat N] [--keep-fixture]";
/// Names a binary to test instead of building one. `cargo xtask headless` reads it too.
pub(crate) const BINARY_ENV: &str = "EXCISE_E2E_BINARY";

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Selection {
    tier: Tier,
    scenarios: Vec<String>,
    profiles: Vec<Profile>,
    repeat: u32,
    keep_fixture: bool,
}

/// Runs the command.
pub fn e2e(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let selection =
        parse(args).map_err(|message| io::Error::other(format!("{message}\n{USAGE}")))?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);

    let scenarios = select_scenarios(
        load_scenarios(&root.join("crates/excise-harness/scenarios"))?,
        &selection.scenarios,
    )?;
    let binary = match env::var_os(BINARY_ENV).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => build_release_binary(&root, &target)?,
    };
    let options = E2eOptions {
        binary,
        tier: selection.tier,
        scenarios,
        named: !selection.scenarios.is_empty(),
        profiles: selection.profiles,
        repeat: selection.repeat,
        keep_fixture: selection.keep_fixture,
        out_root: target.join("excise-e2e"),
        work_dir: None,
        git_sha: super::current_head_sha()?,
    };

    let started = Instant::now();
    let report = run_e2e(&options, |record| {
        eprintln!(
            "  {} [{}] run {}: {} in {:.2}s",
            record.report.scenario,
            record.report.profile,
            record.repetition,
            record.report.verdict,
            record.report.duration.as_secs_f64()
        );
    })?;
    println!("{}", report.table());
    println!("wall time: {:.1}s", started.elapsed().as_secs_f64());
    if report.is_success() {
        Ok(())
    } else {
        Err(io::Error::other("the e2e run has blocking verdicts").into())
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Selection, String> {
    let mut selection = Selection {
        tier: Tier::Full,
        scenarios: Vec::new(),
        profiles: Vec::new(),
        repeat: 1,
        keep_fixture: false,
    };
    let mut tier: Option<Tier> = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--quick" | "--full" | "--nightly" => {
                let requested = match argument.as_str() {
                    "--quick" => Tier::Quick,
                    "--full" => Tier::Full,
                    _ => Tier::Nightly,
                };
                if tier
                    .replace(requested)
                    .is_some_and(|earlier| earlier != requested)
                {
                    return Err(
                        "`--quick`, `--full`, and `--nightly` exclude each other".to_owned()
                    );
                }
            }
            "--scenario" => selection.scenarios.push(value(&mut args, "--scenario")?),
            "--profile" => {
                let text = value(&mut args, "--profile")?;
                let profile = Profile::ALL
                    .iter()
                    .copied()
                    .find(|profile| profile.as_str() == text)
                    .ok_or_else(|| {
                        let known: Vec<&str> = Profile::ALL.iter().map(|p| p.as_str()).collect();
                        format!(
                            "unknown profile `{text}`; the profiles are {}",
                            known.join(", ")
                        )
                    })?;
                selection.profiles.push(profile);
            }
            "--repeat" => {
                let text = value(&mut args, "--repeat")?;
                selection.repeat = text
                    .parse()
                    .ok()
                    .filter(|count| *count > 0)
                    .ok_or_else(|| format!("`--repeat` takes a positive integer, not `{text}`"))?;
            }
            "--keep-fixture" => selection.keep_fixture = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    selection.tier = tier.unwrap_or(Tier::Full);
    Ok(selection)
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("`{flag}` needs a value"))
}

/// Keeps the scenarios named on the command line, or all of them when none was named.
fn select_scenarios(all: Vec<Scenario>, names: &[String]) -> Result<Vec<Scenario>, String> {
    if names.is_empty() {
        return Ok(all);
    }
    if let Some(unknown) = names
        .iter()
        .find(|name| !all.iter().any(|s| &s.name == *name))
    {
        let known: Vec<&str> = all.iter().map(|scenario| scenario.name.as_str()).collect();
        return Err(format!(
            "unknown scenario `{unknown}`; the scenarios are {}",
            known.join(", ")
        ));
    }
    Ok(all
        .into_iter()
        .filter(|scenario| names.contains(&scenario.name))
        .collect())
}

/// Builds the release binary and returns its path.
pub(crate) fn build_release_binary(root: &Path, target: &Path) -> Result<PathBuf, Box<dyn Error>> {
    let cargo = env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let status = Command::new(&cargo)
        .current_dir(root)
        .args(["build", "--release", "--locked", "-p", "excise"])
        .status()
        .map_err(|error| io::Error::other(format!("cannot start {}: {error}", cargo.display())))?;
    if !status.success() {
        return Err(
            io::Error::other(format!("building the release binary failed ({status})")).into(),
        );
    }
    Ok(target
        .join("release")
        .join(format!("excise{}", env::consts::EXE_SUFFIX)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Selection, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_run_everything_once() {
        let selection = parsed(&[]).expect("valid");
        assert_eq!(selection.tier, Tier::Full);
        assert_eq!(selection.repeat, 1);
        assert!(selection.scenarios.is_empty() && selection.profiles.is_empty());
    }

    #[test]
    fn selections_accumulate_and_repeat_the_flags_that_take_lists() {
        let selection = parsed(&[
            "--quick",
            "--scenario",
            "a",
            "--scenario",
            "b",
            "--profile",
            "deterministic",
            "--repeat",
            "20",
            "--keep-fixture",
        ])
        .expect("valid");
        assert_eq!(selection.tier, Tier::Quick);
        assert_eq!(selection.scenarios, ["a", "b"]);
        assert_eq!(selection.profiles, [Profile::Deterministic]);
        assert_eq!(selection.repeat, 20);
        assert!(selection.keep_fixture);
    }

    #[test]
    fn the_nightly_flag_selects_the_nightly_tier() {
        let selection = parsed(&["--nightly"]).expect("valid");
        assert_eq!(selection.tier, Tier::Nightly);
    }

    #[test]
    fn contradictory_or_malformed_arguments_are_refused() {
        for args in [
            &["--quick", "--full"][..],
            &["--quick", "--nightly"],
            &["--full", "--nightly"],
            &["--repeat", "0"],
            &["--repeat", "many"],
            &["--repeat"],
            &["--profile", "loud"],
            &["--tier", "quick"],
        ] {
            assert!(parsed(args).is_err(), "{args:?} must be refused");
        }
        assert_eq!(
            parsed(&["--quick", "--quick"]).expect("same twice").tier,
            Tier::Quick
        );
    }
}
