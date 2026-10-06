//! `cargo xtask sweep`: builds every published `v1.*` tag and the candidate, runs the checks a
//! published release can take against each, and writes the version-by-defect table.
//!
//! This is a thin wrapper, like `e2e` and `headless`: the checks, the paired timings, the table, and
//! the `harness-sweep` document all live in `excise_harness::sweep`; this file parses the command
//! line, builds each ref (see `refs.rs`: its own worktree, its own `CARGO_TARGET_DIR`, and the
//! toolchain its own `rust-toolchain.toml` names), prints the grid, and turns the problems the run
//! found into an exit status.

use std::{
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use excise_harness::{
    fixture::Fixtures,
    report::{SweepTier, SweepToolchain},
    runner::work_base,
    sweep::{BuildInput, IdleWindow, RunDir, SweepOptions, VersionInput, label, run_sweep},
};

use crate::refs::{
    BuildSpec, PlannedRef, RefLayout, ToolchainPolicy, build_ref, plan_ref, release_tags,
};

const USAGE: &str = "usage: cargo xtask sweep [--refs REF...] [--quick|--full] [--fixture ID]... \
                     [--rounds N] [--seed S] [--timeout SECONDS]";

/// `--rounds` when it is not given: the median of five interleaved runs the validation program asks
/// for.
const DEFAULT_ROUNDS: u32 = 5;
/// `--seed` when it is not given: fixed, so an unqualified run is still reproducible.
const DEFAULT_SEED: u64 = 0;
/// `--timeout` when it is not given: how long one scan, or one interface run to COMPLETE, may take
/// before it counts as not finished.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// How fast the slow terminal's output is read, in bytes per second: the 150 KB/s the comparisons
/// of the validation program use.
const DRAIN_BYTES_PER_SEC: u64 = 150_000;
/// Where the sweep's builds and their temporary worktrees live below the target directory.
const BUILDS_DIR: &str = "excise-sweep-builds";
const WORKTREES_DIR: &str = "excise-sweep-worktrees";
/// Where the sweep's runs live below the target directory.
const OUT_DIR: &str = "excise-sweep";

/// What the command line asked for.
#[derive(Debug, Default, PartialEq, Eq)]
struct Selection {
    refs: Vec<String>,
    tier: Option<SweepTier>,
    fixtures: Vec<String>,
    rounds: Option<u32>,
    seed: Option<u64>,
    timeout: Option<Duration>,
}

/// Runs the command.
pub fn sweep(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let selection =
        parse(args).map_err(|message| io::Error::other(format!("{message}\n{USAGE}")))?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    let target = env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);

    let layout = RefLayout::below(&target, BUILDS_DIR, WORKTREES_DIR);
    let refs = if selection.refs.is_empty() {
        let mut refs = release_tags(&root)?;
        refs.push("HEAD".to_owned());
        refs
    } else {
        selection.refs.clone()
    };

    // Everything that can be known before a build is checked before the first one starts: every
    // ref resolves, and every toolchain the refs pin is installed.
    let planning = BuildSpec {
        root: &root,
        layout: &layout,
        policy: ToolchainPolicy::RefPinned,
        log: None,
    };
    let mut planned: Vec<PlannedRef> = Vec::with_capacity(refs.len());
    let mut missing = Vec::new();
    for reference in &refs {
        match plan_ref(&planning, reference) {
            Ok(plan) => planned.push(plan),
            Err(error) => missing.push(error.to_string()),
        }
    }
    if !missing.is_empty() {
        return Err(io::Error::other(format!(
            "cannot sweep before building anything:\n  {}",
            missing.join("\n  ")
        ))
        .into());
    }

    let run = RunDir::create(&target.join(OUT_DIR))?;
    eprintln!(
        "sweep {}: {} versions, candidate {}",
        run.id(),
        planned.len(),
        planned.last().map_or("?", |plan| plan.reference.as_str())
    );
    let mut versions = Vec::with_capacity(planned.len());
    for plan in &planned {
        versions.push(build_version(&root, &layout, &run, plan));
    }

    let tier = selection.tier.unwrap_or(SweepTier::Full);
    let options = SweepOptions {
        versions,
        tier,
        fixture_ids: selection.fixtures,
        rounds: selection.rounds.unwrap_or(DEFAULT_ROUNDS),
        seed: selection.seed.unwrap_or(DEFAULT_SEED),
        timeout: selection.timeout.unwrap_or(DEFAULT_TIMEOUT),
        drain_bytes_per_sec: DRAIN_BYTES_PER_SEC,
        idle: IdleWindow::default(),
        fixtures: Fixtures::bundled(),
        run,
        work_dir: work_base(),
        checkout_sha: super::current_head_sha()?,
    };
    let report = run_sweep(&options, |line| eprintln!("{line}"))?;

    println!("{}", report.grid());
    println!("table:    {}", report.table_path.display());
    println!("document: {}", report.document_path.display());
    let problems = report.problems();
    if problems.is_empty() {
        Ok(())
    } else {
        for problem in &problems {
            eprintln!("problem: {problem}");
        }
        Err(io::Error::other(format!(
            "the sweep ran, but {} thing(s) went wrong; the table says which cells they leave open",
            problems.len()
        ))
        .into())
    }
}

/// Where the build log of a ref is kept below the run's directory: `builds/<label>.log`. The label
/// is the ref with every character a file name should not hold replaced, then the start of its
/// commit (see `excise_harness::sweep::label`), so that no ref can make a subdirectory or leave the
/// run's directory, and the run's evidence for the same version is kept under the same name.
fn build_log_name(reference: &str, sha: &str) -> String {
    format!("builds/{}.log", label(reference, sha))
}

/// Builds one version with the toolchain its own `rust-toolchain.toml` names. A build that fails is
/// a result: the sweep goes on to the others.
fn build_version(root: &Path, layout: &RefLayout, run: &RunDir, plan: &PlannedRef) -> VersionInput {
    let log_name = build_log_name(&plan.reference, &plan.sha);
    let log_path = run.path().join(&log_name);
    let spec = BuildSpec {
        root,
        layout,
        policy: ToolchainPolicy::RefPinned,
        log: Some(&log_path),
    };
    eprintln!(
        "building {} ({}) with {}",
        plan.reference,
        &plan.sha[..12],
        plan.channel.as_deref().unwrap_or("the default toolchain")
    );
    // The commit that was planned, not the ref again: a ref that moved while the earlier versions
    // were built (`HEAD`, after a commit) would otherwise be built at a commit that was never
    // planned, under a log named for the old one and evidence named for the new.
    match build_ref(&spec, &plan.sha) {
        Ok(built) => {
            eprintln!("  built{}", if built.cached { " (cached)" } else { "" });
            VersionInput {
                reference: plan.reference.clone(),
                sha: built.sha,
                toolchain: built.toolchain.map(|toolchain| SweepToolchain {
                    channel: toolchain.channel,
                    rustc: toolchain.rustc,
                }),
                build: BuildInput::Built {
                    binary: built.binary,
                    cached: built.cached,
                    log: log_path.is_file().then_some(log_name),
                },
            }
        }
        Err(error) => {
            eprintln!("  the build failed: {error}");
            VersionInput {
                reference: plan.reference.clone(),
                sha: plan.sha.clone(),
                toolchain: plan.channel.clone().map(|channel| SweepToolchain {
                    rustc: "not run".to_owned(),
                    channel,
                }),
                build: BuildInput::Failed {
                    reason: error.to_string(),
                    log: log_path.is_file().then_some(log_name),
                },
            }
        }
    }
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Selection, String> {
    let mut selection = Selection::default();
    let mut tier: Option<SweepTier> = None;
    let mut pending = args.next();
    while let Some(argument) = pending {
        pending = args.next();
        match argument.as_str() {
            "--quick" | "--full" => {
                let requested = if argument == "--quick" {
                    SweepTier::Quick
                } else {
                    SweepTier::Full
                };
                if tier
                    .replace(requested)
                    .is_some_and(|earlier| earlier != requested)
                {
                    return Err("`--quick` and `--full` exclude each other".to_owned());
                }
            }
            "--refs" => {
                // One or more refs: everything up to the next flag.
                let mut any = false;
                while let Some(next) = pending.take_if(|next| !next.starts_with("--")) {
                    selection.refs.push(next);
                    any = true;
                    pending = args.next();
                }
                if !any {
                    return Err("`--refs` needs at least one ref".to_owned());
                }
            }
            "--fixture" => {
                selection.fixtures.push(
                    pending
                        .take()
                        .ok_or_else(|| "`--fixture` needs a value".to_owned())?,
                );
                pending = args.next();
            }
            "--rounds" => {
                let text = pending
                    .take()
                    .ok_or_else(|| "`--rounds` needs a value".to_owned())?;
                pending = args.next();
                selection.rounds = Some(
                    text.parse()
                        .ok()
                        .filter(|rounds| *rounds > 0)
                        .ok_or_else(|| {
                            format!("`--rounds` takes a positive integer, not `{text}`")
                        })?,
                );
            }
            "--seed" => {
                let text = pending
                    .take()
                    .ok_or_else(|| "`--seed` needs a value".to_owned())?;
                pending = args.next();
                selection.seed =
                    Some(text.parse().map_err(|_| {
                        format!("`--seed` takes an unsigned integer, not `{text}`")
                    })?);
            }
            "--timeout" => {
                let text = pending
                    .take()
                    .ok_or_else(|| "`--timeout` needs a value".to_owned())?;
                pending = args.next();
                let seconds: u64 = text
                    .parse()
                    .ok()
                    .filter(|seconds| *seconds > 0)
                    .ok_or_else(|| {
                        format!("`--timeout` takes a positive number of seconds, not `{text}`")
                    })?;
                selection.timeout = Some(Duration::from_secs(seconds));
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    selection.tier = tier;
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(args: &[&str]) -> Result<Selection, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn no_arguments_sweep_every_release_and_the_head_at_the_full_tier() {
        let selection = parsed(&[]).expect("valid");

        assert!(selection.refs.is_empty());
        assert_eq!(selection.tier, None);
        assert!(selection.fixtures.is_empty());
        assert_eq!(selection.rounds, None);
    }

    #[test]
    fn refs_take_every_value_up_to_the_next_flag_and_may_be_given_again() {
        let selection = parsed(&[
            "--refs", "v1.2.4", "v1.3.0", "HEAD", "--quick", "--refs", "main",
        ])
        .expect("valid");

        assert_eq!(selection.refs, ["v1.2.4", "v1.3.0", "HEAD", "main"]);
        assert_eq!(selection.tier, Some(SweepTier::Quick));
    }

    #[test]
    fn a_ref_may_be_the_last_argument() {
        assert_eq!(
            parsed(&["--rounds", "2", "--refs", "HEAD"])
                .expect("valid")
                .refs,
            ["HEAD"]
        );
    }

    #[test]
    fn flags_override_their_defaults() {
        let selection = parsed(&[
            "--full",
            "--fixture",
            "wide-1k",
            "--fixture",
            "identity-small",
            "--rounds",
            "3",
            "--seed",
            "42",
            "--timeout",
            "60",
        ])
        .expect("valid");

        assert_eq!(selection.tier, Some(SweepTier::Full));
        assert_eq!(selection.fixtures, ["wide-1k", "identity-small"]);
        assert_eq!(selection.rounds, Some(3));
        assert_eq!(selection.seed, Some(42));
        assert_eq!(selection.timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn contradictory_or_malformed_arguments_are_refused() {
        for args in [
            &["--quick", "--full"][..],
            &["--refs"],
            &["--refs", "--quick"],
            &["--rounds", "0"],
            &["--rounds", "many"],
            &["--rounds"],
            &["--seed", "x"],
            &["--timeout", "0"],
            &["--fixture"],
            &["--tier", "quick"],
            &["HEAD"],
        ] {
            assert!(parsed(args).is_err(), "{args:?} must be refused");
        }
        assert_eq!(
            parsed(&["--quick", "--quick"]).expect("same twice").tier,
            Some(SweepTier::Quick)
        );
    }

    #[test]
    fn a_build_log_is_named_by_the_label_and_lies_directly_in_the_builds_directory() {
        let sha = "0123456789abcdef0123456789abcdef01234567";
        let other = "fedcba9876543210fedcba9876543210fedcba98";
        assert_eq!(
            build_log_name("v1.3.0", sha),
            "builds/v1.3.0-0123456789ab.log"
        );

        for reference in [
            "feature/x",
            "a/../b",
            "../../escape",
            "v1.4.0^{/fix}",
            "with space",
            "-rf",
            ".hidden",
            "naïve/日本語",
            "",
        ] {
            let name = build_log_name(reference, sha);

            let parts: Vec<&str> = name.split('/').collect();
            assert_eq!(parts.len(), 2, "{reference:?}: {name}");
            assert_eq!(parts[0], "builds", "{reference:?}: {name}");
            assert!(
                parts[1].ends_with("-0123456789ab.log"),
                "{reference:?}: {name}"
            );
            assert!(
                !parts[1].starts_with(['.', '-']),
                "{reference:?} would be hidden or an option: {name}"
            );
            assert!(
                parts[1]
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')),
                "{reference:?}: {name}"
            );
        }
        // Two refs that read alike once their characters are replaced are told apart by the
        // commit, and one ref at two commits keeps two logs.
        assert_ne!(
            build_log_name("release/1.0", sha),
            build_log_name("release_1.0", other)
        );
        assert_ne!(build_log_name("main", sha), build_log_name("main", other));
    }
}
