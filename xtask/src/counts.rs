//! `cargo xtask counts`: counts what a build of `excise` costs, without timing anything.
//!
//! This is a thin wrapper, like `e2e`, `headless`, and `bench-e2e`: the fixtures, the scans, the
//! interactive session, the check that every count is deterministic, and the document all live in
//! `excise-harness::counts`. This file parses the command line, finds or builds the binary, and
//! says what the commit is.

use std::{
    env,
    error::Error,
    io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use excise_harness::{
    counts::{Commit, CountsOptions, FIXTURES, run_counts},
    fixture::Fixtures,
    report::{MAX_CASES, PullRequestOrigin},
    runner::work_base,
};

use crate::e2e::{BINARY_ENV, build_release_binary};

const USAGE: &str = "usage: cargo xtask counts [--out FILE] [--repeat N] [--fixture ID]... \
                     [--timeout SECONDS] \
                     [--pull-request NUMBER --base-sha SHA --head-sha SHA]";

/// `--repeat` when it is not given: every fixture is counted twice and the counts must agree.
const DEFAULT_REPEAT: u32 = 2;
/// `--timeout` when it is not given: one scan or one session of the largest fixture takes
/// seconds on a hosted runner.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
struct Selection {
    out: Option<PathBuf>,
    repeat: u32,
    fixtures: Vec<String>,
    timeout: Duration,
    pull_request: Option<PullRequestOrigin>,
}

/// Runs the command.
pub fn counts(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
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
    let options = CountsOptions {
        binary,
        fixtures: Fixtures::bundled(),
        fixture_ids: if selection.fixtures.is_empty() {
            FIXTURES.iter().map(|id| (*id).to_owned()).collect()
        } else {
            selection.fixtures
        },
        repeat: selection.repeat,
        timeout: selection.timeout,
        work_dir: work_base(),
        out: selection
            .out
            .unwrap_or_else(|| target.join("excise-counts").join("counts.json")),
        commit: head_commit()?,
        pull_request: selection.pull_request,
    };

    let started = Instant::now();
    let report = run_counts(&options, |progress| {
        eprintln!(
            "  [{}/{}] {} (count {} of {}) ...",
            progress.index, progress.total, progress.fixture, progress.run, progress.repeat
        );
    })?;
    println!("{}", report.table());
    println!("document: {}", report.document_path.display());
    println!("wall time: {:.1}s", started.elapsed().as_secs_f64());
    Ok(())
}

/// The commit at `HEAD` and when it was committed.
fn head_commit() -> Result<Commit, Box<dyn Error>> {
    let sha = super::current_head_sha()?;
    let output = Command::new("git")
        .args(["show", "-s", "--format=%cI", "HEAD"])
        .output()
        .map_err(|error| io::Error::other(format!("could not run `git show`: {error}")))?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "`git show -s --format=%cI HEAD` failed ({})",
            output.status
        ))
        .into());
    }
    let committed_at = String::from_utf8(output.stdout)
        .map_err(|error| io::Error::other(format!("`git show` returned invalid UTF-8: {error}")))?
        .trim()
        .to_owned();
    Ok(Commit { sha, committed_at })
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Selection, String> {
    let mut selection = Selection {
        out: None,
        repeat: DEFAULT_REPEAT,
        fixtures: Vec::new(),
        timeout: DEFAULT_TIMEOUT,
        pull_request: None,
    };
    let mut number: Option<u64> = None;
    let mut base_sha: Option<String> = None;
    let mut head_sha: Option<String> = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--out" => selection.out = Some(PathBuf::from(value(&mut args, "--out")?)),
            "--repeat" => {
                let text = value(&mut args, "--repeat")?;
                selection.repeat = text
                    .parse()
                    .ok()
                    .filter(|count| *count > 0)
                    .ok_or_else(|| format!("`--repeat` takes a positive integer, not `{text}`"))?;
            }
            "--fixture" => selection.fixtures.push(value(&mut args, "--fixture")?),
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
            "--pull-request" => {
                let text = value(&mut args, "--pull-request")?;
                number = Some(
                    text.parse()
                        .ok()
                        .filter(|number| (1..=excise_harness::report::MAX_COUNT).contains(number))
                        .ok_or_else(|| {
                            format!("`--pull-request` takes a pull request number, not `{text}`")
                        })?,
                );
            }
            "--base-sha" => base_sha = Some(commit_argument(&mut args, "--base-sha")?),
            "--head-sha" => head_sha = Some(commit_argument(&mut args, "--head-sha")?),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    if selection.fixtures.len() > MAX_CASES {
        return Err(format!(
            "`--fixture` names {} fixtures, but a counts document holds at most {MAX_CASES}: \
             count fewer in one run",
            selection.fixtures.len()
        ));
    }
    selection.pull_request = match (number, base_sha, head_sha) {
        (None, None, None) => None,
        (Some(number), Some(base_sha), Some(head_sha)) => Some(PullRequestOrigin {
            number,
            base_sha,
            head_sha,
        }),
        _ => {
            return Err(
                "`--pull-request`, `--base-sha`, and `--head-sha` go together: give all three or \
                 none"
                    .to_owned(),
            );
        }
    };
    Ok(selection)
}

pub(crate) fn value(args: &mut impl Iterator<Item = String>, flag: &str) -> Result<String, String> {
    args.next().ok_or_else(|| format!("`{flag}` needs a value"))
}

/// The value of a flag that names a commit: all 40 lowercase hexadecimal digits.
pub(crate) fn commit_argument(
    args: &mut impl Iterator<Item = String>,
    flag: &str,
) -> Result<String, String> {
    let text = value(args, flag)?;
    if text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(text)
    } else {
        Err(format!(
            "`{flag}` takes a full commit of 40 lowercase hexadecimal digits"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA_A: &str = "0123456789abcdef0123456789abcdef01234567";
    const SHA_B: &str = "fedcba9876543210fedcba9876543210fedcba98";

    fn parsed(args: &[&str]) -> Result<Selection, String> {
        parse(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn more_fixtures_than_a_document_holds_are_refused_before_anything_is_built() {
        let fixtures = |count: usize| -> Vec<String> {
            (0..count)
                .flat_map(|index| ["--fixture".to_owned(), format!("fixture-{index:02}")])
                .collect()
        };
        let parse_fixtures = |count: usize| parse(fixtures(count).into_iter());

        let error = parse_fixtures(17).expect_err("17 do not fit one document");
        assert!(
            error.contains("17 fixtures") && error.contains("at most 16"),
            "{error}"
        );
        assert_eq!(
            parse_fixtures(16).expect("16 is the limit").fixtures.len(),
            16
        );
    }

    #[test]
    fn no_arguments_count_the_fixed_set_twice_for_no_pull_request() {
        let selection = parsed(&[]).expect("valid");

        assert_eq!(selection.repeat, 2);
        assert!(selection.fixtures.is_empty() && selection.out.is_none());
        assert_eq!(selection.pull_request, None);
    }

    #[test]
    fn a_pull_request_takes_its_number_and_both_commits_together() {
        let selection = parsed(&[
            "--pull-request",
            "123",
            "--base-sha",
            SHA_A,
            "--head-sha",
            SHA_B,
            "--out",
            "counts.json",
            "--fixture",
            "wide-1k",
            "--repeat",
            "3",
        ])
        .expect("valid");

        assert_eq!(
            selection.pull_request,
            Some(PullRequestOrigin {
                number: 123,
                base_sha: SHA_A.to_owned(),
                head_sha: SHA_B.to_owned(),
            })
        );
        assert_eq!(selection.out, Some(PathBuf::from("counts.json")));
        assert_eq!(selection.fixtures, ["wide-1k"]);
        assert_eq!(selection.repeat, 3);
    }

    #[test]
    fn malformed_or_incomplete_arguments_are_refused() {
        for args in [
            &["--pull-request", "123"][..],
            &["--base-sha", SHA_A],
            &["--pull-request", "123", "--base-sha", SHA_A],
            &[
                "--pull-request",
                "0",
                "--base-sha",
                SHA_A,
                "--head-sha",
                SHA_B,
            ],
            &[
                "--pull-request",
                "-1",
                "--base-sha",
                SHA_A,
                "--head-sha",
                SHA_B,
            ],
            &[
                "--pull-request",
                "x",
                "--base-sha",
                SHA_A,
                "--head-sha",
                SHA_B,
            ],
            &[
                "--pull-request",
                "1",
                "--base-sha",
                "abc",
                "--head-sha",
                SHA_B,
            ],
            &[
                "--pull-request",
                "1",
                "--base-sha",
                &SHA_A.to_uppercase(),
                "--head-sha",
                SHA_B,
            ],
            &["--repeat", "0"],
            &["--repeat", "many"],
            &["--timeout", "0"],
            &["--out"],
            &["--quick"],
        ] {
            assert!(parsed(args).is_err(), "{args:?} must be refused");
        }
    }
}
