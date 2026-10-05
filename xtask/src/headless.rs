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
    fixture::{FixtureCache, Fixtures, PrivilegedOptIn},
    headless::{Class, DEFAULT_REPEAT, Expectations, Progress, SuiteOptions, run_suite},
    report::Tier,
    scenario::Profile,
};

use crate::e2e::{BINARY_ENV, build_release_binary};

const USAGE: &str = "usage: cargo xtask headless [--quick|--full] [--fixture ID]... \
                     [--fixture-dir DIR] [--class scale|identity|hostile|volumes]... \
                     [--profile default|deterministic] [--repeat N] [--timeout SECONDS] \
                     [--timing-informational] [--keep-scratch]";

/// How long one scan or one `du` may take by default. A scan of tens of thousands of entries
/// takes minutes today.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(900);

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Selection {
    tier: Tier,
    fixtures: Vec<String>,
    fixture_dir: Option<PathBuf>,
    classes: Vec<Class>,
    profile: Profile,
    repeat: u32,
    timeout: Duration,
    timing_informational: bool,
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
        fixtures: fixtures_in(selection.fixture_dir.as_deref())?,
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
        // A spec of a directory of the operator's own has no documented defect: it must pass.
        expectations: if selection.fixture_dir.is_some() {
            Expectations::none()
        } else {
            Expectations::bundled()?
        },
        timing_informational: selection.timing_informational,
    };

    let started = Instant::now();
    let report = run_suite(&options, |progress| match progress {
        Progress::Started {
            fixture,
            index,
            total,
        } => eprintln!("  [{index}/{total}] {fixture} ..."),
        Progress::Finished(report) => {
            let warned = match report.timing_warnings.len() {
                0 => String::new(),
                count => format!(", {count} timing warning(s)"),
            };
            eprintln!(
                "  {}: {} in {:.1}s{warned}",
                report.fixture,
                report.verdict,
                report.duration.as_secs_f64()
            );
        }
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
        fixture_dir: None,
        tier: Tier::Full,
        fixtures: Vec::new(),
        classes: Vec::new(),
        profile: Profile::Default,
        repeat: DEFAULT_REPEAT,
        timeout: DEFAULT_TIMEOUT,
        keep_scratch: false,
        timing_informational: false,
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
            "--fixture-dir" => {
                selection.fixture_dir = Some(PathBuf::from(value(&mut args, "--fixture-dir")?));
            }
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
            "--timing-informational" => selection.timing_informational = true,
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    selection.tier = tier.unwrap_or(Tier::Full);
    Ok(selection)
}

/// The fixtures a run takes its specs from: the specs of `dir`, a directory of the operator's own
/// (`--fixture-dir`), or the ones the harness ships. Generated masters are cached below the target
/// directory either way, under a name that holds the spec's hash, so two specs of one id never
/// share a cache entry.
///
/// What the directory holds is not trusted to be specs. The harness reads each spec when it is
/// asked for one (`headless` asks for every spec of the directory, `bench-e2e` for those of the
/// ids it is given): the file is opened without following a link, must be a regular file, and is
/// read up to `MAX_SPEC_BYTES`, so a link, a FIFO, a folder, or a file above the cap is refused
/// there with a message, and is never waited on or read in full.
///
/// # Errors
///
/// Returns an error when `dir` is not a directory.
pub fn fixtures_in(dir: Option<&Path>) -> io::Result<Fixtures> {
    match dir {
        None => Ok(Fixtures::bundled()),
        Some(dir) if dir.is_dir() => Ok(Fixtures::new(dir, FixtureCache::in_target_dir())),
        Some(dir) => Err(io::Error::other(format!(
            "`--fixture-dir {}` is not a directory",
            dir.display()
        ))),
    }
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
    fn timing_is_gated_unless_the_flag_makes_it_informational() {
        assert!(!parsed(&["--full"]).expect("valid").timing_informational);

        let selection =
            parsed(&["--timing-informational", "--full", "--repeat", "3"]).expect("valid");
        assert!(selection.timing_informational);
        assert_eq!(selection.tier, Tier::Full, "the flag takes no value");
        assert_eq!(selection.repeat, 3);
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

    #[test]
    fn a_fixture_directory_comes_with_ids_and_must_be_a_directory() {
        let selection =
            parsed(&["--fixture-dir", "specs", "--fixture", "home-50k"]).expect("valid");
        assert_eq!(selection.fixture_dir.as_deref(), Some(Path::new("specs")));
        assert_eq!(selection.fixtures, ["home-50k"]);
        assert!(parsed(&[]).expect("valid").fixture_dir.is_none());
        assert!(
            parsed(&["--fixture-dir"]).is_err(),
            "the option needs a directory"
        );
        assert!(fixtures_in(None).is_ok());
        let error = fixtures_in(Some(Path::new("/no/such/directory/of/specs")))
            .expect_err("a directory that is not there is refused");
        assert!(error.to_string().contains("is not a directory"), "{error}");
    }

    /// `headless` and `bench-e2e` both take their fixtures from `fixtures_in`, so a spec of a
    /// `--fixture-dir` that is a link is refused for both: the harness does not follow it.
    #[cfg(unix)]
    #[test]
    fn a_spec_of_a_fixture_directory_that_is_a_link_is_refused_and_not_followed() {
        /// A directory of the temporary area that is removed when it drops.
        struct Scratch(PathBuf);

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let scratch =
            Scratch(env::temp_dir().join(format!("xtask-fixture-dir-{}", std::process::id())));
        std::fs::create_dir_all(&scratch.0).expect("a scratch directory");
        std::fs::write(
            scratch.0.join("backing.toml"),
            "schema_version = 1\nid = \"linked\"\ndescription = \"A spec.\"\nseed = 1\n\n\
             [[parts]]\nkind = \"file\"\nroot = \"f.bin\"\nsize = 8\n",
        )
        .expect("a spec");
        std::os::unix::fs::symlink("backing.toml", scratch.0.join("linked.toml")).expect("a link");

        let fixtures = fixtures_in(Some(&scratch.0)).expect("a directory");
        let error = fixtures
            .spec("linked")
            .expect_err("a link is not a spec of the directory");

        assert!(error.to_string().contains("symbolic link"), "{error}");
    }
}
