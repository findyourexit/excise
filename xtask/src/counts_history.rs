//! `cargo xtask counts-record` and `cargo xtask counts-comment`: the two ends of the count
//! history.
//!
//! Thin wrappers over `excise-harness::counts`, like `counts` itself. `counts-record` appends one
//! record to the `bench-data` branch; `counts-comment` reads a pull request's counts, which are
//! untrusted input, and the history, which is a directory that the caller has already checked
//! out, and writes the comment that the workflow posts. Nothing here talks to GitHub's API: that
//! is the workflow's, which is given only a file of Markdown, a pull request number, and the base
//! commit that the comparison was made against, all of which this has checked (the posting script
//! checks them against the pull request). `counts-comment` never reads a token, so it can be run,
//! as the workflow does, in an environment that has none.

use std::{
    env,
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use excise_harness::counts::{
    artifact::read_untrusted,
    comment::render,
    history::{
        ANCESTOR_LIMIT, AppendOptions, Appended, Auth, BaseSearch, DEFAULT_BRANCH, ancestors,
        append_record, find_base,
    },
};

use crate::counts::{commit_argument, value};

const RECORD_USAGE: &str = "usage: cargo xtask counts-record --record FILE --remote URL \
                            [--branch NAME] [--attempts N]";
const COMMENT_USAGE: &str = "usage: cargo xtask counts-comment --artifact FILE \
                             --expect-head-sha SHA --out DIR [--history DIR] [--repo DIR]";

/// `--attempts` when it is not given: a push that loses a race is tried again this many times in
/// all.
const DEFAULT_ATTEMPTS: u32 = 5;
/// How long the second try waits; each later one waits a unit longer.
const RETRY_DELAY: Duration = Duration::from_secs(5);

/// What `counts-record` was asked to do.
#[derive(Debug, PartialEq, Eq)]
struct RecordSelection {
    record: PathBuf,
    remote: String,
    branch: String,
    attempts: u32,
}

/// What `counts-comment` was asked to do.
#[derive(Debug, PartialEq, Eq)]
struct CommentSelection {
    artifact: PathBuf,
    expect_head_sha: String,
    out: PathBuf,
    /// A checkout of the history branch that is already on disk.
    history: Option<PathBuf>,
    repo: PathBuf,
}

/// Appends a record to the history branch.
pub fn record(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let selection = parse_record(args)
        .map_err(|message| io::Error::other(format!("{message}\n{RECORD_USAGE}")))?;
    let document = read_untrusted(&selection.record)?;
    let target = target_dir()?;
    let work_dir = target.join("excise-counts").join("history");
    fs::create_dir_all(&work_dir)?;
    // The token is read here and goes only into the environment of the git commands, never into
    // an argument or a line of output.
    let auth = env::var("GITHUB_TOKEN")
        .ok()
        .filter(|token| !token.is_empty())
        .and_then(|token| Auth::for_token(&selection.remote, &token));
    let outcome = append_record(
        &AppendOptions {
            remote: selection.remote,
            branch: selection.branch.clone(),
            auth,
            attempts: selection.attempts,
            retry_delay: RETRY_DELAY,
            work_dir,
        },
        &document,
    )?;
    let commit = &document.context.git_sha[..12];
    match outcome {
        Appended::Recorded { branch_created } => println!(
            "recorded the {} counts of {commit} on `{}`{}",
            document.context.runner.os,
            selection.branch,
            if branch_created {
                " (the branch did not exist: created it)"
            } else {
                ""
            }
        ),
        Appended::AlreadyRecorded => println!(
            "`{}` already has a record of {commit} for {}: left as it is",
            selection.branch, document.context.runner.os
        ),
    }
    Ok(())
}

/// Renders the comment for a pull request's counts.
pub fn comment(args: impl Iterator<Item = String>) -> Result<(), Box<dyn Error>> {
    let selection = parse_comment(args)
        .map_err(|message| io::Error::other(format!("{message}\n{COMMENT_USAGE}")))?;
    let document = read_untrusted(&selection.artifact)?;
    let Some(pull_request) = &document.context.pull_request else {
        return Err(io::Error::other("the document is not the counts of a pull request").into());
    };
    if pull_request.head_sha != selection.expect_head_sha {
        return Err(io::Error::other(format!(
            "the counts are of the pull request head {}, but the run that uploaded them was of {}",
            pull_request.head_sha, selection.expect_head_sha
        ))
        .into());
    }

    let chain = match ancestors(&selection.repo, &pull_request.base_sha, ANCESTOR_LIMIT) {
        Ok(chain) => chain,
        Err(error) => {
            eprintln!("note: the history of the base commit cannot be walked: {error}");
            Vec::new()
        }
    };
    // The history is a checkout of the branch that the caller made (the workflow takes one out of
    // its own checkout, so that this needs no token), or nothing where there is none yet.
    let search = match selection
        .history
        .as_deref()
        .filter(|history| history.is_dir())
    {
        Some(history) => find_base(history, &document.context.runner.os, &chain),
        None => BaseSearch {
            found: None,
            searched: chain.len(),
            rejected: Vec::new(),
        },
    };
    for rejected in &search.rejected {
        eprintln!(
            "note: the record of {} was passed over: {}",
            rejected.commit, rejected.reason
        );
    }

    let comment = render(&document, &search);
    fs::create_dir_all(&selection.out)?;
    fs::write(selection.out.join("comment.md"), &comment.body)?;
    fs::write(
        selection.out.join("pull-request-number"),
        format!("{}\n", pull_request.number),
    )?;
    // What the comparison was made against, for the posting script to check against the pull
    // request's own base: the artifact chose it, and the schema has held it to 40 hex digits.
    fs::write(
        selection.out.join("base-sha"),
        format!("{}\n", pull_request.base_sha),
    )?;
    match &search.found {
        Some(found) => println!(
            "compared with the record of {} ({} commit(s) before the base): {} counts, {} \
             changed, {} flagged",
            &found.commit[..12],
            found.distance,
            comment.compared,
            comment.changed,
            comment.flagged
        ),
        None => println!(
            "no record found among {} commit(s) of the base's history: nothing to compare with",
            search.searched
        ),
    }
    println!(
        "wrote {} ({} bytes)",
        selection.out.join("comment.md").display(),
        comment.body.len()
    );
    Ok(())
}

fn target_dir() -> Result<PathBuf, Box<dyn Error>> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .ok_or_else(|| io::Error::other("the xtask manifest has no parent directory"))?
        .to_path_buf();
    Ok(env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from))
}

fn parse_record(mut args: impl Iterator<Item = String>) -> Result<RecordSelection, String> {
    let mut record = None;
    let mut remote = None;
    let mut branch = DEFAULT_BRANCH.to_owned();
    let mut attempts = DEFAULT_ATTEMPTS;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--record" => record = Some(PathBuf::from(value(&mut args, "--record")?)),
            "--remote" => remote = Some(value(&mut args, "--remote")?),
            "--branch" => branch = value(&mut args, "--branch")?,
            "--attempts" => {
                let text = value(&mut args, "--attempts")?;
                attempts = text
                    .parse()
                    .ok()
                    .filter(|count| *count > 0)
                    .ok_or_else(|| {
                        format!("`--attempts` takes a positive integer, not `{text}`")
                    })?;
            }
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(RecordSelection {
        record: record.ok_or("`--record` is required")?,
        remote: remote.ok_or("`--remote` is required")?,
        branch,
        attempts,
    })
}

fn parse_comment(mut args: impl Iterator<Item = String>) -> Result<CommentSelection, String> {
    let mut artifact = None;
    let mut expect_head_sha = None;
    let mut out = None;
    let mut history = None;
    let mut repo = PathBuf::from(".");
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--artifact" => artifact = Some(PathBuf::from(value(&mut args, "--artifact")?)),
            "--expect-head-sha" => {
                expect_head_sha = Some(commit_argument(&mut args, "--expect-head-sha")?);
            }
            "--out" => out = Some(PathBuf::from(value(&mut args, "--out")?)),
            "--history" => history = Some(PathBuf::from(value(&mut args, "--history")?)),
            "--repo" => repo = PathBuf::from(value(&mut args, "--repo")?),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(CommentSelection {
        artifact: artifact.ok_or("`--artifact` is required")?,
        expect_head_sha: expect_head_sha.ok_or("`--expect-head-sha` is required")?,
        out: out.ok_or("`--out` is required")?,
        history,
        repo,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn args(text: &[&str]) -> impl Iterator<Item = String> {
        text.iter()
            .map(|arg| (*arg).to_owned())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn a_record_needs_its_file_and_its_remote_and_defaults_the_branch() {
        let selection = parse_record(args(&[
            "--record",
            "r.json",
            "--remote",
            "https://example.test/o/r",
        ]))
        .expect("valid");

        assert_eq!(selection.branch, "bench-data");
        assert_eq!(selection.attempts, 5);
        assert!(parse_record(args(&["--record", "r.json"])).is_err());
        assert!(parse_record(args(&["--remote", "x"])).is_err());
        for bad in [&["--attempts", "0"][..], &["--attempts", "x"], &["--bogus"]] {
            let mut all = vec!["--record", "r.json", "--remote", "x"];
            all.extend_from_slice(bad);
            assert!(parse_record(args(&all)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_comment_needs_the_trusted_head_commit_and_takes_only_a_full_one() {
        let selection = parse_comment(args(&[
            "--artifact",
            "pr-counts/counts.json",
            "--expect-head-sha",
            SHA,
            "--out",
            "out",
            "--history",
            "history",
        ]))
        .expect("valid");

        assert_eq!(selection.expect_head_sha, SHA);
        assert_eq!(selection.history, Some(PathBuf::from("history")));
        assert_eq!(selection.repo, PathBuf::from("."));
        assert!(
            parse_comment(args(&["--artifact", "a", "--out", "o"])).is_err(),
            "the trusted commit is required: without it a stale artifact could be believed"
        );
        assert!(
            parse_comment(args(&[
                "--artifact",
                "a",
                "--out",
                "o",
                "--expect-head-sha",
                "main"
            ]))
            .is_err()
        );
        for token_reading_option in ["--remote", "--branch"] {
            assert!(
                parse_comment(args(&[
                    "--artifact",
                    "a",
                    "--out",
                    "o",
                    "--expect-head-sha",
                    SHA,
                    token_reading_option,
                    "x"
                ]))
                .is_err(),
                "{token_reading_option}: the history is a directory the caller checked out, \
                 so this process never needs a token to read it"
            );
        }
    }
}
