//! `cargo xtask headless`: scans the fixtures without a terminal, holds every scan report to the
//! oracle of its fixture, and times the scan against `du -sk`.
//!
//! This is a thin wrapper. Fixtures, the scan, the oracle diff, the `du` reference, the expected
//! failures, and the run summary all live in `excise-harness`; this file parses the command line,
//! finds or builds the binary, and turns the verdicts into an exit status.

use std::{
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use excise_harness::{
    fixture::{Fixtures, PrivilegedOptIn},
    headless::{Class, DEFAULT_REPEAT, Expectations, Progress, SuiteOptions, run_suite},
    report::Tier,
    scenario::Profile,
};

use crate::e2e::{BINARY_ENV, build_release_binary};

const USAGE: &str = "usage: cargo xtask headless [--quick|--full] [--fixture ID]... \
                     [--class scale|identity|hostile|volumes]... \
                     [--profile default|deterministic] [--repeat N] [--timeout SECONDS] \
                     [--keep-scratch]";

/// How long one scan or one `du` may take by default. A scan of tens of thousands of entries
/// takes minutes today.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(900);

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Selection {
    tier: Tier,
    fixtures: Vec<String>,
    classes: Vec<Class>,
    profile: Profile,
    repeat: u32,
    timeout: Duration,
    keep_scratch: bool,
}

/// Runs the command.
pub fn headless(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let selection =
        parse(args).map_err(|message| io::Error::other(format!("{message}\n{USAGE}")))?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);

    let binary = match env::var_os(BINARY_ENV).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => build_release_binary(&root, &target)?,
    };
    let options = SuiteOptions {
        binary,
        fixtures: Fixtures::bundled(),
        tier: selection.tier,
        fixture_ids: selection.fixtures,
        classes: selection.classes,
        profile: selection.profile,
        repeat: selection.repeat,
        timeout: selection.timeout,
        keep: selection.keep_scratch,
        out_root: target.join("excise-headless"),
        work_dir: None,
        git_sha: super::current_head_sha()?,
        privileged: PrivilegedOptIn::from_env(),
        expectations: Expectations::bundled()?,
    };

    let started = Instant::now();
    let report = run_suite(&options, |progress| match progress {
        Progress::Started {
            fixture,
            index,
            total,
        } => eprintln!("  [{index}/{total}] {fixture} ..."),
        Progress::Finished(report) => eprintln!(
            "  {}: {} in {:.1}s",
            report.fixture,
            report.verdict,
            report.duration.as_secs_f64()
        ),
    })?;
    println!("{}", report.table());
    println!("wall time: {:.1}s", started.elapsed().as_secs_f64());
    if report.is_success() {
        Ok(())
    } else {
        Err(io::Error::other("the headless run has blocking verdicts").into())
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Selection, String> {
    let mut selection = Selection {
        tier: Tier::Full,
        fixtures: Vec::new(),
        classes: Vec::new(),
        profile: Profile::Default,
        repeat: DEFAULT_REPEAT,
        timeout: DEFAULT_TIMEOUT,
        keep_scratch: false,
    };
    let mut tier: Option<Tier> = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--quick" | "--full" => {
                let requested = if argument == "--quick" {
                    Tier::Quick
                } else {
                    Tier::Full
                };
                if tier
                    .replace(requested)
                    .is_some_and(|earlier| earlier != requested)
                {
                    return Err("`--quick` and `--full` exclude each other".to_owned());
                }
            }
            "--fixture" => selection.fixtures.push(value(&mut args, "--fixture")?),
            "--class" => {
                let text = value(&mut args, "--class")?;
                let class = Class::ALL
                    .iter()
                    .copied()
                    .find(|class| class.as_str() == text)
                    .ok_or_else(|| {
                        let known: Vec<&str> = Class::ALL.iter().map(|c| c.as_str()).collect();
                        format!(
                            "unknown class `{text}`; the classes are {}",
                            known.join(", ")
                        )
                    })?;
                selection.classes.push(class);
            }
            "--profile" => {
                let text = value(&mut args, "--profile")?;
                selection.profile = [Profile::Default, Profile::Deterministic]
                    .into_iter()
                    .find(|profile| profile.as_str() == text)
                    .ok_or_else(|| {
                        format!(
                            "unknown profile `{text}`; a headless scan runs under `default` or \
                             `deterministic`"
                        )
                    })?;
            }
            "--repeat" => {
                let text = value(&mut args, "--repeat")?;
                selection.repeat = text.parse().map_err(|_| {
                    format!("`--repeat` takes a whole number of pairs, not `{text}`")
                })?;
            }
            "--timeout" => {
                let text = value(&mut args, "--timeout")?;
                let seconds: u64 = text
                    .parse()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| {
                        format!("`--timeout` takes a positive number of seconds, not `{text}`")
                    })?;
                selection.timeout = Duration::from_secs(seconds);
            }
            "--keep-scratch" => selection.keep_scratch = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    selection.tier = tier.unwrap_or(Tier::Full);
    Ok(selection)
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("`{flag}` needs a value"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Selection, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_run_the_full_tier_with_five_pairs_under_the_default_profile() {
        let selection = parsed(&[]).expect("valid");

        assert_eq!(selection.tier, Tier::Full);
        assert_eq!(selection.repeat, 5);
        assert_eq!(selection.profile, Profile::Default);
        assert!(selection.fixtures.is_empty() && selection.classes.is_empty());
        assert!(!selection.keep_scratch);
    }

    #[test]
    fn selections_accumulate_and_the_flags_that_take_lists_repeat() {
        let selection = parsed(&[
            "--quick",
            "--fixture",
            "wide-1k",
            "--fixture",
            "identity-small",
            "--class",
            "hostile",
            "--profile",
            "deterministic",
            "--repeat",
            "0",
            "--timeout",
            "30",
            "--keep-scratch",
        ])
        .expect("valid");

        assert_eq!(selection.tier, Tier::Quick);
        assert_eq!(selection.fixtures, ["wide-1k", "identity-small"]);
        assert_eq!(selection.classes, [Class::Hostile]);
        assert_eq!(selection.profile, Profile::Deterministic);
        assert_eq!(selection.repeat, 0, "zero pairs only checks");
        assert_eq!(selection.timeout, Duration::from_secs(30));
        assert!(selection.keep_scratch);
    }

    #[test]
    fn contradictory_or_malformed_arguments_are_refused() {
        for args in [
            &["--quick", "--full"][..],
            &["--repeat", "many"],
            &["--repeat"],
            &["--timeout", "0"],
            &["--profile", "narrow"],
            &["--class", "huge"],
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
