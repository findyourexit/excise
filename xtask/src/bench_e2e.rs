//! `cargo xtask bench-e2e`: paired, interleaved A/B evidence between two builds.
//!
//! This is a thin wrapper, like `e2e` and `headless`: the comparison engine, the bootstrap
//! statistics, the verdict policy, and the context all live in `excise_harness::bench`; this file
//! parses the command line, builds or locates the baseline and candidate binaries (the baseline
//! through `refs::build_planned`, in a temporary detached worktree, with the toolchain of this
//! environment and with its output in a log file), prints the verdict table, and turns the result
//! into an exit status.

use std::{
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use excise_harness::{
    bench::{BenchOptions, Case, run_bench_e2e},
    fixture::Fixtures,
    runner::{default_limit, load_scenarios, work_base},
    scenario::{Budget, Profile},
};

use crate::{
    e2e::build_release_binary,
    headless::fixtures_in,
    refs::{BuildSpec, RefLayout, ToolchainPolicy, build_planned, is_cached, plan_ref},
};

const USAGE: &str = "usage: cargo xtask bench-e2e --baseline <ref> [--baseline-binary PATH] \
                     [--candidate-binary PATH] [--fixture ID]... [--fixture-dir DIR] \
                     [--scenario NAME --profile PROFILE]... [--pairs N] [--seed S] \
                     [--timing-threshold FRACTION] [--memory-tolerance FRACTION] [--strict] \
                     [--timeout SECONDS]";

/// `--pairs` when it is not given.
const DEFAULT_PAIRS: u32 = 10;
/// `--seed` when it is not given: fixed, so an unqualified run is still reproducible.
const DEFAULT_SEED: u64 = 0;
/// `--timeout` when it is not given: generous enough for a 1,000,000-entry fixture (F1 measured
/// about 556 s wall on the per-run-flush baseline).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(900);
/// Where the baselines are cached, directly below the target directory: `<sha>/` below it is the
/// target directory of the build of one commit, and its `release/excise` is the baseline binary.
const BASELINES_DIR: &str = "excise-bench-e2e-baselines";
/// Where the temporary worktrees of the baseline builds are made, directly below the target
/// directory.
const WORKTREES_DIR: &str = "excise-bench-e2e-worktrees";
/// The file the output of a baseline build goes to, directly in the baselines directory and in no
/// `<sha>` directory below it. Every build truncates it.
const BASELINE_LOG: &str = "baseline-build.log";

/// What the command line asked for.
#[derive(Debug, Default)]
struct Selection {
    baseline_ref: Option<String>,
    baseline_binary: Option<PathBuf>,
    candidate_binary: Option<PathBuf>,
    fixtures: Vec<String>,
    fixture_dir: Option<PathBuf>,
    /// `(scenario name, profile)`, in the order `--scenario` was given; the profile is filled in
    /// by the `--profile` that follows it.
    scenarios: Vec<(String, Option<Profile>)>,
    pairs: Option<u32>,
    seed: Option<u64>,
    timing_threshold: Option<f64>,
    memory_tolerance: Option<f64>,
    strict: bool,
    timeout: Option<Duration>,
}

/// Runs the command.
pub fn bench_e2e(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let selection =
        parse(args).map_err(|message| io::Error::other(format!("{message}\n{USAGE}")))?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);

    let (baseline_binary, baseline_ref) = resolve_baseline(&selection, &root, &target)?;
    let candidate_binary = match &selection.candidate_binary {
        Some(path) => path.clone(),
        None => build_release_binary(&root, &target)?,
    };
    let candidate_ref = current_candidate_ref(&root)?;
    let scan_fixtures = fixtures_in(selection.fixture_dir.as_deref())?;
    let cases = resolve_cases(&selection, &root, &scan_fixtures)?;

    let options = BenchOptions {
        baseline_binary,
        baseline_ref,
        candidate_binary,
        candidate_ref,
        fixtures: scan_fixtures,
        scenario_fixtures: Fixtures::bundled(),
        cases,
        pairs: selection.pairs.unwrap_or(DEFAULT_PAIRS),
        seed: selection.seed.unwrap_or(DEFAULT_SEED),
        timing_threshold: selection
            .timing_threshold
            .unwrap_or_else(|| default_limit(Budget::TimingAbRegression).unwrap_or(0.20)),
        memory_tolerance: selection
            .memory_tolerance
            .unwrap_or_else(|| default_limit(Budget::MemoryAbTolerance).unwrap_or(0.05)),
        strict: selection.strict,
        timeout: selection.timeout.unwrap_or(DEFAULT_TIMEOUT),
        work_dir: work_base(),
        out_root: target.join("excise-bench-e2e"),
    };

    let report = run_bench_e2e(&options)?;
    println!(
        "baseline  {} ({})",
        options.baseline_ref,
        &report.document.baseline.binary_sha256[..12]
    );
    println!(
        "candidate {} ({})",
        options.candidate_ref,
        &report.document.candidate.binary_sha256[..12]
    );
    println!("{}", report.table());
    println!("document: {}", report.document_path.display());
    if report.is_success() {
        Ok(())
    } else {
        Err(io::Error::other("the comparison has one or more blocking verdicts").into())
    }
}

/// Where a baseline is built, and where its build writes what it prints: the layout of the shared
/// ref builder below the target directory, and the log of the build.
struct BaselineBuild {
    layout: RefLayout,
    log: PathBuf,
}

impl BaselineBuild {
    /// The baseline build below `target`: the cache is `target/excise-bench-e2e-baselines/`, the
    /// temporary worktrees are made in `target/excise-bench-e2e-worktrees/`, and the log is
    /// `baseline-build.log` directly in the cache directory.
    fn below(target: &Path) -> Self {
        let layout = RefLayout::below(target, BASELINES_DIR, WORKTREES_DIR);
        let log = layout.builds.join(BASELINE_LOG);
        Self { layout, log }
    }

    /// The spec of the baseline build of the repository at `root`: with the toolchain of this
    /// environment, so that the baseline is compiled by the compiler of the candidate, and with
    /// the output of the build in the log and never on the terminal. The build runs in a process
    /// group of its own, which is not the terminal's foreground group, and a write to the
    /// terminal would stop it where the terminal sets `tostop`.
    fn spec<'a>(&'a self, root: &'a Path) -> BuildSpec<'a> {
        BuildSpec {
            root,
            layout: &self.layout,
            policy: ToolchainPolicy::Inherited,
            log: Some(&self.log),
        }
    }
}

/// The baseline binary and its `git_ref` label: `--baseline <ref>`, resolved once by
/// `refs::plan_ref` and then built, at the commit it resolved to, by `refs::build_planned` in a
/// temporary detached worktree, with the toolchain of this environment so that the baseline is
/// compiled by the same compiler as the candidate, with its output in
/// `target/excise-bench-e2e-baselines/baseline-build.log`, and cached by commit SHA under
/// `target/excise-bench-e2e-baselines/`; or `--baseline-binary` taken as-is (labeled with
/// `--baseline` if that was also given, else a `binary:<path>` placeholder).
fn resolve_baseline(
    selection: &Selection,
    root: &Path,
    target: &Path,
) -> Result<(PathBuf, String), Box<dyn Error>> {
    if let Some(path) = &selection.baseline_binary {
        let label = selection
            .baseline_ref
            .clone()
            .unwrap_or_else(|| format!("binary:{}", path.display()));
        return Ok((path.clone(), label));
    }
    let reference = selection
        .baseline_ref
        .clone()
        .expect("parse requires --baseline when --baseline-binary is absent");
    let baseline = BaselineBuild::below(target);
    let spec = baseline.spec(root);
    // The ref is resolved once, here, and the plan is what is announced and what is built: asking
    // for the ref again could resolve a branch that has moved since, so that a build nobody was
    // told of starts, or an announced one does not run. The build's output goes to the log, so
    // the person who waits for it is told where it is, and only when a build runs: a baseline
    // that is cached builds nothing.
    let plan = plan_ref(&spec, &reference)?;
    if !is_cached(&spec, &plan)? {
        eprintln!(
            "building the baseline {reference}; its output is in {}",
            baseline.log.display()
        );
    }
    let built = build_planned(&spec, &plan)?;
    Ok((built.binary, built.sha))
}

/// The cases named on the command line: every `--fixture` (checked against the ids of `fixtures`,
/// the bundled specs or those of `--fixture-dir`) and every `--scenario`/`--profile` pair
/// (checked against the known scenarios and the profiles each one declares).
fn resolve_cases(
    selection: &Selection,
    root: &Path,
    fixtures: &Fixtures,
) -> Result<Vec<Case>, Box<dyn Error>> {
    let known_fixtures = fixtures.ids()?;
    for id in &selection.fixtures {
        if !known_fixtures.contains(id) {
            return Err(io::Error::other(format!(
                "unknown fixture `{id}`; the fixtures are {}",
                known_fixtures.join(", ")
            ))
            .into());
        }
    }
    let mut cases: Vec<Case> = selection
        .fixtures
        .iter()
        .cloned()
        .map(|id| Case::Fixture { id })
        .collect();
    if selection.scenarios.is_empty() {
        return Ok(cases);
    }
    let all_scenarios = load_scenarios(&root.join("crates/excise-harness/scenarios"))?;
    for (name, profile) in &selection.scenarios {
        let scenario = all_scenarios
            .iter()
            .find(|candidate| &candidate.name == name)
            .ok_or_else(|| {
                let known: Vec<&str> = all_scenarios.iter().map(|s| s.name.as_str()).collect();
                io::Error::other(format!(
                    "unknown scenario `{name}`; the scenarios are {}",
                    known.join(", ")
                ))
            })?
            .clone();
        let profile = profile.expect("parse requires a --profile for every --scenario");
        if !scenario.profiles.contains(&profile) {
            let declared: Vec<&str> = scenario
                .profiles
                .iter()
                .copied()
                .map(Profile::as_str)
                .collect();
            return Err(io::Error::other(format!(
                "scenario `{name}` does not declare the `{profile}` profile; it declares {}",
                declared.join(", ")
            ))
            .into());
        }
        cases.push(Case::Scenario {
            scenario: Box::new(scenario),
            profile,
        });
    }
    Ok(cases)
}

#[allow(clippy::too_many_lines)]
fn parse(mut args: impl Iterator<Item = String>) -> Result<Selection, String> {
    let mut selection = Selection::default();
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--baseline" => selection.baseline_ref = Some(value(&mut args, "--baseline")?),
            "--baseline-binary" => {
                selection.baseline_binary =
                    Some(PathBuf::from(value(&mut args, "--baseline-binary")?));
            }
            "--candidate-binary" => {
                selection.candidate_binary =
                    Some(PathBuf::from(value(&mut args, "--candidate-binary")?));
            }
            "--fixture" => selection.fixtures.push(value(&mut args, "--fixture")?),
            "--fixture-dir" => {
                selection.fixture_dir = Some(PathBuf::from(value(&mut args, "--fixture-dir")?));
            }
            "--scenario" => selection
                .scenarios
                .push((value(&mut args, "--scenario")?, None)),
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
                let last = selection
                    .scenarios
                    .last_mut()
                    .ok_or_else(|| "`--profile` must follow a `--scenario`".to_owned())?;
                if last.1.is_some() {
                    return Err(format!(
                        "scenario `{}` already has a profile; give `--scenario` again to pair \
                         another profile with it",
                        last.0
                    ));
                }
                last.1 = Some(profile);
            }
            "--pairs" => {
                let text = value(&mut args, "--pairs")?;
                selection.pairs = Some(
                    text.parse()
                        .ok()
                        .filter(|pairs| *pairs > 0)
                        .ok_or_else(|| {
                            format!("`--pairs` takes a positive integer, not `{text}`")
                        })?,
                );
            }
            "--seed" => {
                let text = value(&mut args, "--seed")?;
                selection.seed =
                    Some(text.parse().map_err(|_| {
                        format!("`--seed` takes an unsigned integer, not `{text}`")
                    })?);
            }
            "--timing-threshold" => {
                let text = value(&mut args, "--timing-threshold")?;
                selection.timing_threshold =
                    Some(text.parse().map_err(|_| {
                        format!("`--timing-threshold` takes a fraction, not `{text}`")
                    })?);
            }
            "--memory-tolerance" => {
                let text = value(&mut args, "--memory-tolerance")?;
                selection.memory_tolerance =
                    Some(text.parse().map_err(|_| {
                        format!("`--memory-tolerance` takes a fraction, not `{text}`")
                    })?);
            }
            "--strict" => selection.strict = true,
            "--timeout" => {
                let text = value(&mut args, "--timeout")?;
                let seconds: u64 = text.parse().map_err(|_| {
                    format!("`--timeout` takes a positive integer of seconds, not `{text}`")
                })?;
                selection.timeout = Some(Duration::from_secs(seconds));
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if let Some((name, None)) = selection
        .scenarios
        .iter()
        .find(|(_, profile)| profile.is_none())
    {
        return Err(format!("`--scenario {name}` has no `--profile`"));
    }
    if selection.fixtures.is_empty() && selection.scenarios.is_empty() {
        return Err("nothing to compare: pass --fixture or --scenario at least once".to_owned());
    }
    if selection.baseline_ref.is_none() && selection.baseline_binary.is_none() {
        return Err("give --baseline <ref> or --baseline-binary <path>".to_owned());
    }
    Ok(selection)
}

fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("`{flag}` needs a value"))
}

/// `HEAD`'s commit, with a `-dirty` suffix when the working tree has uncommitted changes: the
/// candidate is always "the current checkout's release build" (scope), whether or not
/// `--candidate-binary` skipped building it.
fn current_candidate_ref(root: &Path) -> Result<String, Box<dyn Error>> {
    let sha = super::current_head_sha()?;
    let status = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| io::Error::other(format!("could not run `git status`: {error}")))?;
    if !status.status.success() {
        return Err(io::Error::other("`git status --porcelain` failed").into());
    }
    Ok(if status.stdout.is_empty() {
        sha
    } else {
        format!("{sha}-dirty")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Selection, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn a_baseline_ref_and_at_least_one_case_is_enough() {
        let selection = parsed(&["--baseline", "main", "--fixture", "wide-1k"]).expect("valid");
        assert_eq!(selection.baseline_ref.as_deref(), Some("main"));
        assert_eq!(selection.fixtures, ["wide-1k"]);
        assert!(selection.scenarios.is_empty());
    }

    #[test]
    fn a_baseline_binary_alone_is_also_enough() {
        let selection = parsed(&[
            "--baseline-binary",
            "/tmp/old-excise",
            "--fixture",
            "wide-1k",
        ])
        .expect("valid");
        assert_eq!(
            selection.baseline_binary,
            Some(PathBuf::from("/tmp/old-excise"))
        );
        assert!(selection.baseline_ref.is_none());
    }

    #[test]
    fn scenario_and_profile_pair_up_in_the_order_given() {
        let selection = parsed(&[
            "--baseline",
            "main",
            "--scenario",
            "delete-folder-lifecycle",
            "--profile",
            "default",
            "--scenario",
            "navigate-and-quit",
            "--profile",
            "deterministic",
        ])
        .expect("valid");
        assert_eq!(
            selection.scenarios,
            [
                ("delete-folder-lifecycle".to_owned(), Some(Profile::Default)),
                ("navigate-and-quit".to_owned(), Some(Profile::Deterministic)),
            ]
        );
    }

    #[test]
    fn every_scenario_needs_its_own_profile_and_nothing_else_is_required() {
        for args in [
            &["--baseline", "main", "--scenario", "s"][..],
            &["--baseline", "main", "--profile", "default"],
            &["--baseline", "main"],
            &["--fixture", "wide-1k"],
            &[
                "--baseline",
                "main",
                "--fixture",
                "unknown-flag-value",
                "--bogus",
            ],
            &["--baseline", "main", "--pairs", "0"],
            &["--baseline", "main", "--seed", "not-a-number"],
        ] {
            assert!(parsed(args).is_err(), "{args:?} must be refused");
        }
    }

    #[test]
    fn defaults_are_left_unset_for_the_caller_to_fill_in() {
        let selection = parsed(&["--baseline", "main", "--fixture", "f"]).expect("valid");
        assert_eq!(selection.pairs, None);
        assert_eq!(selection.seed, None);
        assert!(!selection.strict);
        assert_eq!(selection.timeout, None);
    }

    #[test]
    fn flags_override_their_defaults() {
        let selection = parsed(&[
            "--baseline",
            "main",
            "--fixture",
            "f",
            "--pairs",
            "3",
            "--seed",
            "42",
            "--timing-threshold",
            "0.3",
            "--memory-tolerance",
            "0.1",
            "--strict",
            "--timeout",
            "60",
        ])
        .expect("valid");
        assert_eq!(selection.pairs, Some(3));
        assert_eq!(selection.seed, Some(42));
        assert_eq!(selection.timing_threshold, Some(0.3));
        assert_eq!(selection.memory_tolerance, Some(0.1));
        assert!(selection.strict);
        assert_eq!(selection.timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn the_baseline_build_logs_directly_in_the_baselines_directory_whatever_the_target() {
        for target in [
            "target",
            "/work/target",
            "../shared/target",
            "a b/target",
            ".",
        ] {
            let baseline = BaselineBuild::below(Path::new(target));
            let spec = baseline.spec(Path::new("/repo"));

            // The output of the build goes to a log, never to the terminal. The log is a file
            // directly in the cache directory: in no `<sha>` directory below it, which a build
            // uses as its target directory.
            let log = spec.log.expect("the baseline build has a log");
            assert_eq!(
                log.parent(),
                Some(baseline.layout.builds.as_path()),
                "{target}"
            );
            assert_eq!(
                log.file_name().and_then(|name| name.to_str()),
                Some("baseline-build.log"),
                "{target}"
            );
            // The documented paths are kept: the cache and the worktrees are where they were.
            assert_eq!(
                baseline.layout.builds,
                Path::new(target).join("excise-bench-e2e-baselines"),
                "{target}"
            );
            assert_eq!(
                baseline.layout.worktrees,
                Path::new(target).join("excise-bench-e2e-worktrees"),
                "{target}"
            );
            // The same builder as before: the toolchain of this environment, in the repository
            // that was asked for.
            assert_eq!(spec.policy, ToolchainPolicy::Inherited);
            assert_eq!(spec.root, Path::new("/repo"));
            assert_eq!(spec.layout, &baseline.layout);
        }
    }
}
