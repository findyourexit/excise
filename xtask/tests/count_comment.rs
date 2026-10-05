//! `cargo xtask counts-comment`, run as the comment workflow runs it: the real binary, a pull
//! request's counts as an artifact, and a checkout of the history branch.
//!
//! The artifact is the one input that the pull request's author controls, and the posting step
//! takes the comment and the pull request number that this writes on trust. So what is checked
//! here is what the command writes for an artifact that is what it says it is, and that it writes
//! nothing, and says so in one line, for each that is not.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU32, Ordering},
};

use excise_harness::counts::{comment::MARKER, history::record_path};

const XTASK: &str = env!("CARGO_BIN_EXE_xtask");
/// The head of the pull request, as the triggering run's payload names it.
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_HEAD: &str = "fedcba9876543210fedcba9876543210fedcba98";
const HASH: &str = "3a7bd3e2360a3d29eea436fcfb7e44c735d117c42d1c1835420b6b9942dd4f1b";
const TOOLCHAIN: &str = "rustc 1.98.0 (88d9e12ae 2026-08-18)";
const MERGE: &str = "89abcdef0123456789abcdef0123456789abcdef";

/// The counts document of `commit` on Linux: one fixture whose scan store held `store` bytes. It
/// is a pull request's when `pull_request` gives the base and head commits.
fn document(commit: &str, store: u64, pull_request: Option<(&str, &str)>) -> String {
    let pull_request = pull_request.map_or_else(String::new, |(base, head)| {
        format!(r#","pull_request":{{"number":7,"base_sha":"{base}","head_sha":"{head}"}}"#)
    });
    format!(
        r#"{{
  "document_kind": "harness-counts",
  "schema_version": 1,
  "context": {{
    "git_sha": "{commit}",
    "committed_at": "2026-10-05T12:34:52+11:00",
    "runner": {{ "os": "linux", "os_version": "Ubuntu 24.04", "arch": "x86_64" }},
    "toolchain": "{TOOLCHAIN}"{pull_request}
  }},
  "cases": [
    {{
      "fixture": {{ "id": "wide-1k", "hash": "{HASH}", "seed": 7 }},
      "profile": "deterministic",
      "metrics": {{ "entries": 1002, "scan_store_bytes": {store} }}
    }}
  ]
}}
"#
    )
}

/// Runs git in `dir` and returns what it printed.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.test"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// One test's directory: a repository of three commits, a history checkout, and the artifact.
struct Scene {
    dir: PathBuf,
    /// The commits of the repository, oldest first.
    commits: [String; 3],
}

impl Scene {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "count-comment-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).expect("a repository directory");
        git(&repo, &["init", "-q", "-b", "main"]);
        let commits = ["one", "two", "three"].map(|message| {
            git(&repo, &["commit", "-q", "--allow-empty", "-m", message]);
            git(&repo, &["rev-parse", "HEAD"])
        });
        Self { dir, commits }
    }

    /// The commit the pull request was based on.
    fn base(&self) -> &str {
        &self.commits[2]
    }

    fn history(&self) -> PathBuf {
        self.dir.join("history")
    }

    fn out(&self) -> PathBuf {
        self.dir.join("out")
    }

    /// Files the record of `commit` as the history branch does.
    fn record(&self, commit: &str, store: u64) {
        let path = self
            .history()
            .join(record_path("linux", commit).expect("a record name"));
        fs::create_dir_all(path.parent().expect("a directory")).expect("the record's directory");
        fs::write(path, document(commit, store, None)).expect("a record");
    }

    /// A file of the artifact's directory.
    fn file(&self, name: &str, text: &str) -> PathBuf {
        let directory = self.dir.join("artifact");
        fs::create_dir_all(&directory).expect("the artifact's directory");
        let path = directory.join(name);
        fs::write(&path, text).expect("a file");
        path
    }

    /// The pull request's counts: a merge commit whose scan store held `store` bytes.
    fn counts(&self, store: u64) -> String {
        document(MERGE, store, Some((self.base(), HEAD)))
    }

    fn run(&self, artifact: &Path, expect_head: &str, history: &Path) -> Output {
        Command::new(XTASK)
            .arg("counts-comment")
            .arg("--artifact")
            .arg(artifact)
            .args(["--expect-head-sha", expect_head])
            .arg("--history")
            .arg(history)
            .arg("--repo")
            .arg(self.dir.join("repo"))
            .arg("--out")
            .arg(self.out())
            .env_remove("GITHUB_TOKEN")
            .env_remove("GH_TOKEN")
            .output()
            .expect("xtask runs")
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_pull_requests_counts_become_a_comment_against_the_nearest_recorded_ancestor() {
    let scene = Scene::new();
    // The base commit has no record; its parent has: one commit earlier.
    scene.record(&scene.commits[1], 350_000);
    let artifact = scene.file("counts.json", &scene.counts(385_000));

    let output = scene.run(&artifact, HEAD, &scene.history());

    assert!(output.status.success(), "{}", stderr(&output));
    let comment = fs::read_to_string(scene.out().join("comment.md")).expect("the comment");
    assert!(comment.starts_with(MARKER), "{comment}");
    assert!(
        comment.contains("the nearest recorded ancestor of its base commit"),
        "{comment}"
    );
    assert!(
        comment.contains("+35,000 (+10.0%), worse"),
        "a rise of 10% is flagged:\n{comment}"
    );
    assert_eq!(
        fs::read_to_string(scene.out().join("pull-request-number")).expect("the number"),
        "7\n"
    );
    assert_eq!(
        fs::read_to_string(scene.out().join("base-sha")).expect("the base commit"),
        format!("{}\n", scene.base()),
        "the posting script is given the base the comparison was made against, to check"
    );
}

#[test]
fn a_history_that_does_not_exist_yet_still_gets_a_comment_that_says_so() {
    let scene = Scene::new();
    let artifact = scene.file("counts.json", &scene.counts(385_000));

    let output = scene.run(&artifact, HEAD, &scene.history());

    assert!(output.status.success(), "{}", stderr(&output));
    let comment = fs::read_to_string(scene.out().join("comment.md")).expect("the comment");
    assert!(
        comment.contains("so there is nothing to compare them with"),
        "{comment}"
    );
    assert!(!comment.contains("worse"), "{comment}");
}

#[test]
fn counts_that_are_of_another_head_than_the_run_that_uploaded_them_are_refused() {
    let scene = Scene::new();
    scene.record(&scene.commits[2], 350_000);
    let artifact = scene.file("counts.json", &scene.counts(385_000));

    let output = scene.run(&artifact, OTHER_HEAD, &scene.history());

    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("the run that uploaded them"),
        "{}",
        stderr(&output)
    );
    assert!(
        !scene.out().exists(),
        "an artifact that belongs to another run writes no comment"
    );
}

#[test]
fn an_artifact_that_is_not_what_it_says_it_is_is_refused_in_one_line_and_writes_nothing() {
    let scene = Scene::new();
    scene.record(&scene.commits[2], 350_000);
    let valid = scene.counts(385_000);
    let real = scene.file("real.json", &valid);
    let link = scene.dir.join("artifact").join("link.json");
    symlink(&real, &link).expect("a link");
    let artifacts = [
        (
            "larger than the bound",
            scene.file("big.json", &format!("{valid}{}", " ".repeat(70_000))),
        ),
        (
            "not the schema",
            scene.file("schema.json", r#"{"document_kind": "harness-counts"}"#),
        ),
        (
            "a workflow command in a string",
            scene.file(
                "command.json",
                &valid.replace(TOOLCHAIN, "x\\n::set-output name=pwn::1"),
            ),
        ),
        ("a link to a valid document", link),
        (
            "the counts of a commit, not of a pull request",
            scene.file("record.json", &document(MERGE, 385_000, None)),
        ),
    ];

    for (what, artifact) in artifacts {
        let output = scene.run(&artifact, HEAD, &scene.history());

        assert!(!output.status.success(), "{what} was accepted");
        let said = stderr(&output);
        assert!(
            said.starts_with("verification failed: ") && said.trim_end().lines().count() == 1,
            "{what}: one line was expected, got {said:?}"
        );
        assert!(!scene.out().exists(), "{what}: something was written");
    }
}
