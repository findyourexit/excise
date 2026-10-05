//! The script that posts the pull-request count comment, run against a stand-in for `gh`.
//!
//! The script is the one place that writes to a pull request, and its inputs come from a
//! workflow's payload and from an artifact that a pull request's author controls. These tests run
//! it with a `gh` that serves canned responses and records every request, so that what it asks
//! GitHub for, and what it refuses to, is checked without a network.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU32, Ordering},
};

use excise_harness::counts::comment::MARKER;

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../.github/scripts/post-count-comment.sh"
);
const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER_HEAD: &str = "fedcba9876543210fedcba9876543210fedcba98";
/// Where the counted run's head came from, as the workflow's payload says.
const HEAD_REPOSITORY: &str = "o/r";
const HEAD_BRANCH: &str = "probe/pr-same";
/// The base commit that the counted run's counts were compared against.
const BASE: &str = "89abcdef0123456789abcdef0123456789abcdef";
const OTHER_BASE: &str = "76543210fedcba9876543210fedcba9876543210";

/// A `gh` that answers only the requests the script is meant to make, and says so loudly for any
/// other: anything unexpected exits non-zero.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$FAKE_DIR/calls"
[[ "$1" == api ]] || { echo "unexpected gh command: $*" >&2; exit 64; }
shift
method=GET
path=
body=
while (( $# )); do
  case "$1" in
    --method) method="$2"; shift 2 ;;
    --field) body="${2#body=@}"; shift 2 ;;
    --paginate) shift ;;
    *) path="$1"; shift ;;
  esac
done
case "$method $path" in
  "GET repos/o/r/pulls/7") cat "$FAKE_DIR/pull.json" ;;
  "GET repos/o/r/issues/7/comments?per_page=100") cat "$FAKE_DIR/comments.json" ;;
  "PATCH repos/o/r/issues/comments/"* | "POST repos/o/r/issues/7/comments")
    cp "$body" "$FAKE_DIR/posted"
    echo '{}'
    ;;
  *) echo "unexpected request: $method $path" >&2; exit 65 ;;
esac
"#;

/// A directory for one test: the stand-in `gh`, the canned responses, and the body to post.
struct Scene(PathBuf);

impl Scene {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "post-count-comment-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).expect("a directory");
        fs::write(dir.join("bin/gh"), FAKE_GH).expect("the stand-in gh");
        fs::set_permissions(dir.join("bin/gh"), fs::Permissions::from_mode(0o755))
            .expect("executable");
        fs::write(
            dir.join("body.md"),
            format!("{MARKER}\n### Deterministic counts\n"),
        )
        .expect("a body");
        let scene = Self(dir);
        scene.pull("open", HEAD);
        scene.comments("[]");
        scene
    }

    /// The pull request: its state and head commit, from the counted repository and branch,
    /// based on the counted base commit.
    fn pull(&self, state: &str, head: &str) {
        self.pull_from(state, head, Some(HEAD_REPOSITORY), HEAD_BRANCH, BASE);
    }

    /// The pull request, whose head repository is `repository` (`None` where that repository
    /// has been deleted, which the API reports as null), whose head branch is `branch`, and which
    /// is now based on `base`.
    fn pull_from(
        &self,
        state: &str,
        head: &str,
        repository: Option<&str>,
        branch: &str,
        base: &str,
    ) {
        let repo = repository.map_or_else(
            || "null".to_owned(),
            |name| format!(r#"{{"full_name": "{name}"}}"#),
        );
        fs::write(
            self.0.join("pull.json"),
            format!(
                r#"{{"state": "{state}", "head": {{"sha": "{head}", "ref": "{branch}", "repo": {repo}}}, "base": {{"sha": "{base}"}}}}"#
            ),
        )
        .expect("a pull request");
    }

    fn comments(&self, json: &str) {
        fs::write(self.0.join("comments.json"), json).expect("comments");
    }

    /// Runs the script with the usual inputs, `overrides` replacing or adding to them.
    fn run(&self, overrides: &[(&str, &str)]) -> Output {
        self.run_without(overrides, &[])
    }

    /// Runs the script with the usual inputs, `overrides` replacing or adding to them and the
    /// variables named in `unset` not set at all.
    fn run_without(&self, overrides: &[(&str, &str)], unset: &[&str]) -> Output {
        let mut command = Command::new("bash");
        command.arg(SCRIPT).current_dir(&self.0);
        let path = format!(
            "{}:{}",
            self.0.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .env("PATH", path)
            .env("FAKE_DIR", &self.0)
            .env("GH_TOKEN", "unused")
            .env("REPOSITORY", "o/r")
            .env("PR_NUMBER", "7")
            .env("EXPECTED_HEAD_SHA", HEAD)
            .env("HEAD_REPOSITORY", HEAD_REPOSITORY)
            .env("HEAD_BRANCH", HEAD_BRANCH)
            .env("BASE_SHA", BASE)
            .env("RUN_PULL_REQUESTS", "7")
            .env("BODY_FILE", self.0.join("body.md"));
        for (name, value) in overrides {
            command.env(name, value);
        }
        for name in unset {
            command.env_remove(name);
        }
        command.output().expect("bash runs the script")
    }

    /// The requests the script made, one per line.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.0.join("calls"))
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn posted(&self) -> Option<String> {
        fs::read_to_string(self.0.join("posted")).ok()
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn ok(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn bot_comment(id: u64, body: &str) -> String {
    format!(
        r#"{{"id": {id}, "user": {{"login": "github-actions[bot]"}}, "body": {}}}"#,
        serde_json::to_string(body).expect("a string")
    )
}

fn needs_jq() {
    assert!(
        Command::new("jq")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success()),
        "these tests run the script, which needs jq (it is on every hosted runner)"
    );
}

#[test]
fn the_first_comment_is_created_with_the_body_from_the_file() {
    needs_jq();
    let scene = Scene::new();

    let output = scene.run(&[]);

    assert!(ok(&output).contains("Commented on pull request #7."));
    let body = scene.0.join("body.md");
    assert_eq!(
        scene.calls(),
        [
            "api repos/o/r/pulls/7".to_owned(),
            "api --paginate repos/o/r/issues/7/comments?per_page=100".to_owned(),
            format!(
                "api --method POST repos/o/r/issues/7/comments --field body=@{}",
                body.display()
            ),
        ]
    );
    assert_eq!(
        scene.posted(),
        fs::read_to_string(scene.0.join("body.md")).ok()
    );
}

#[test]
fn an_earlier_comment_of_the_bot_is_updated_not_duplicated() {
    needs_jq();
    let scene = Scene::new();
    scene.comments(&format!(
        "[{}, {}]",
        bot_comment(50, "something else the bot said"),
        bot_comment(111, &format!("{MARKER}\nold counts"))
    ));

    let output = scene.run(&[]);

    assert!(ok(&output).contains("Updated comment 111 on pull request #7."));
    let calls = scene.calls();
    assert_eq!(calls.len(), 3, "{calls:?}");
    assert!(
        calls[2].starts_with("api --method PATCH repos/o/r/issues/comments/111 "),
        "{calls:?}"
    );
}

#[test]
fn the_earlier_comment_is_found_on_a_later_page() {
    needs_jq();
    let scene = Scene::new();
    // `gh api --paginate` prints one array per page, one after another.
    scene.comments(&format!(
        "[{}]\n[{}]\n",
        bot_comment(1, "page one"),
        bot_comment(222, &format!("{MARKER}\nold"))
    ));

    ok(&scene.run(&[]));

    assert!(
        scene.calls()[2].contains("PATCH repos/o/r/issues/comments/222"),
        "{:?}",
        scene.calls()
    );
}

#[test]
fn a_comment_that_begins_with_the_marker_but_is_not_the_bots_is_never_edited() {
    needs_jq();
    let scene = Scene::new();
    scene.comments(&format!(
        r#"[{{"id": 5, "user": {{"login": "mallory"}}, "body": {}}}, {{"id": 6, "user": {{"login": "github-actions"}}, "body": {}}}]"#,
        serde_json::to_string(&format!("{MARKER}\nforged")).expect("a string"),
        serde_json::to_string(&format!("{MARKER}\nalmost the bot")).expect("a string"),
    ));

    ok(&scene.run(&[]));

    assert!(
        scene.calls()[2].starts_with("api --method POST repos/o/r/issues/7/comments"),
        "{:?}",
        scene.calls()
    );
}

#[test]
fn a_comment_of_the_bot_that_does_not_begin_with_the_marker_is_not_ours() {
    needs_jq();
    let scene = Scene::new();
    scene.comments(&format!(
        "[{}]",
        bot_comment(9, &format!("mentions {MARKER} later"))
    ));

    ok(&scene.run(&[]));

    assert!(
        scene.calls()[2].contains("--method POST"),
        "{:?}",
        scene.calls()
    );
}

#[test]
fn a_pull_request_that_is_closed_or_has_moved_on_gets_no_comment() {
    needs_jq();
    for (state, head, said) in [
        ("closed", HEAD, "is not open"),
        (
            "open",
            OTHER_HEAD,
            "has moved past the commit that was counted",
        ),
    ] {
        let scene = Scene::new();
        scene.pull(state, head);

        let output = scene.run(&[]);

        assert!(ok(&output).contains(said), "{state} {head}");
        assert_eq!(
            scene.calls(),
            ["api repos/o/r/pulls/7"],
            "nothing but the check was asked"
        );
        assert_eq!(scene.posted(), None);
    }
}

#[test]
fn a_pull_request_from_another_repository_or_branch_than_the_counted_run_gets_no_comment() {
    needs_jq();
    // The same head commit can be the head of more than one open pull request: a contributor's
    // and a maintainer's, say. The commit alone does not say which one was counted.
    for (what, repository, branch) in [
        ("another repository", Some("mallory/r"), HEAD_BRANCH),
        ("another branch", Some(HEAD_REPOSITORY), "another-branch"),
        ("a head repository that was deleted", None, HEAD_BRANCH),
    ] {
        let scene = Scene::new();
        scene.pull_from("open", HEAD, repository, branch, BASE);

        let output = scene.run(&[]);

        assert!(
            ok(&output).contains("is not from the repository and branch that were counted"),
            "{what}"
        );
        assert_eq!(
            scene.calls(),
            ["api repos/o/r/pulls/7"],
            "{what}: nothing but the check was asked"
        );
        assert_eq!(scene.posted(), None, "{what}");
    }
}

#[test]
fn a_pull_request_based_on_another_commit_than_the_counted_base_gets_no_comment() {
    needs_jq();
    // The counts were compared with the record of the base they were taken against. A base that
    // is not the pull request's own now (it moved after the run began, or the artifact named a
    // commit of its choosing) says nothing about what this pull request costs.
    let scene = Scene::new();
    scene.pull_from("open", HEAD, Some(HEAD_REPOSITORY), HEAD_BRANCH, OTHER_BASE);

    let output = scene.run(&[]);

    assert!(
        ok(&output).contains("has a different base commit than the one that was counted"),
        "{output:?}"
    );
    assert_eq!(
        scene.calls(),
        ["api repos/o/r/pulls/7"],
        "nothing but the check was asked"
    );
    assert_eq!(scene.posted(), None);
}

#[test]
fn the_counts_may_only_name_a_pull_request_that_the_runs_payload_lists() {
    needs_jq();
    // GitHub lists the pull requests of a run that is in this repository, and leaves the list
    // empty for a fork's. A listed run is for those pull requests, and an artifact that names
    // another is not what the run made. One head can be the head of several (to different
    // bases), so the list can hold more than one.
    for (listed, comments) in [
        ("7", true),
        ("7,8", true),
        ("8,7", true),
        ("", true),
        ("8", false),
        ("8,9", false),
    ] {
        let scene = Scene::new();

        let output = scene.run(&[("RUN_PULL_REQUESTS", listed)]);

        if comments {
            assert!(
                ok(&output).contains("Commented on pull request #7."),
                "{listed:?}"
            );
        } else {
            assert!(!output.status.success(), "{listed:?} was accepted");
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("not one of the pull requests this run was for"),
                "{listed:?}: {output:?}"
            );
            assert!(
                scene.calls().is_empty(),
                "{listed:?}: the check is made before GitHub is asked: {:?}",
                scene.calls()
            );
            assert_eq!(scene.posted(), None, "{listed:?}");
        }
    }
}

#[test]
fn a_workflow_that_does_not_pass_the_runs_pull_requests_is_refused_not_skipped() {
    needs_jq();
    // Empty is a valid answer (a fork's run). Not passing it at all is a workflow that has
    // lost the check, and it must be seen.
    let scene = Scene::new();

    let output = scene.run_without(&[], &["RUN_PULL_REQUESTS"]);

    assert!(!output.status.success());
    assert!(scene.calls().is_empty(), "{:?}", scene.calls());
}

#[test]
fn every_input_is_checked_before_anything_is_asked_of_github() {
    needs_jq();
    let injected = Path::new(env!("CARGO_TARGET_TMPDIR")).join("pwned");
    let _ = fs::remove_file(&injected);
    let injection = format!("7$(touch {})", injected.display());
    let cases: Vec<(&str, String)> = vec![
        ("PR_NUMBER", String::new()),
        ("PR_NUMBER", "0".to_owned()),
        ("PR_NUMBER", "07".to_owned()),
        ("PR_NUMBER", "-7".to_owned()),
        ("PR_NUMBER", "7 8".to_owned()),
        ("PR_NUMBER", "7;true".to_owned()),
        ("PR_NUMBER", "../7".to_owned()),
        ("PR_NUMBER", "1e3".to_owned()),
        ("PR_NUMBER", "7\n8".to_owned()),
        ("PR_NUMBER", "1234567890123456789".to_owned()),
        ("PR_NUMBER", injection),
        ("EXPECTED_HEAD_SHA", String::new()),
        ("EXPECTED_HEAD_SHA", HEAD.to_uppercase()),
        ("EXPECTED_HEAD_SHA", HEAD[..39].to_owned()),
        ("EXPECTED_HEAD_SHA", "main".to_owned()),
        ("REPOSITORY", String::new()),
        ("HEAD_REPOSITORY", String::new()),
        ("HEAD_REPOSITORY", "o".to_owned()),
        ("HEAD_REPOSITORY", "o/r/extra".to_owned()),
        ("HEAD_REPOSITORY", "o/r;true".to_owned()),
        (
            "HEAD_REPOSITORY",
            format!("o/r$(touch {})", injected.display()),
        ),
        ("HEAD_BRANCH", String::new()),
        ("HEAD_BRANCH", "main\nmore".to_owned()),
        ("HEAD_BRANCH", "a\u{1b}[2J".to_owned()),
        ("REPOSITORY", "o".to_owned()),
        ("REPOSITORY", "o/r/extra".to_owned()),
        ("REPOSITORY", "o/r;true".to_owned()),
        ("REPOSITORY", "../x/y".to_owned()),
        ("BODY_FILE", String::new()),
        ("BODY_FILE", "/definitely/not/a/file".to_owned()),
        ("BASE_SHA", String::new()),
        ("BASE_SHA", BASE.to_uppercase()),
        ("BASE_SHA", BASE[..39].to_owned()),
        ("BASE_SHA", "main".to_owned()),
        ("BASE_SHA", format!("{BASE}$(touch {})", injected.display())),
        ("RUN_PULL_REQUESTS", "07".to_owned()),
        ("RUN_PULL_REQUESTS", "0".to_owned()),
        ("RUN_PULL_REQUESTS", "7,".to_owned()),
        ("RUN_PULL_REQUESTS", ",7".to_owned()),
        ("RUN_PULL_REQUESTS", "7,,8".to_owned()),
        ("RUN_PULL_REQUESTS", "7 8".to_owned()),
        ("RUN_PULL_REQUESTS", "7;true".to_owned()),
        ("RUN_PULL_REQUESTS", "7\n8".to_owned()),
        (
            "RUN_PULL_REQUESTS",
            format!("7,$(touch {})", injected.display()),
        ),
    ];
    for (name, value) in cases {
        let scene = Scene::new();

        let output = scene.run(&[(name, value.as_str())]);

        assert!(!output.status.success(), "{name}={value:?} was accepted");
        assert!(
            scene.calls().is_empty(),
            "{name}={value:?} reached gh: {:?}",
            scene.calls()
        );
    }
    assert!(!injected.exists(), "a value was run as a command");
}

#[test]
fn a_body_that_is_a_link_or_a_directory_is_refused() {
    needs_jq();
    let scene = Scene::new();
    std::os::unix::fs::symlink(scene.0.join("body.md"), scene.0.join("link.md")).expect("a link");

    let linked = scene.run(&[(
        "BODY_FILE",
        scene.0.join("link.md").to_str().expect("UTF-8"),
    )]);
    let directory = scene.run(&[("BODY_FILE", scene.0.to_str().expect("UTF-8"))]);

    assert!(!linked.status.success() && !directory.status.success());
    assert!(scene.calls().is_empty());
}

#[test]
fn the_marker_the_script_looks_for_is_the_one_the_renderer_writes() {
    let script = fs::read_to_string(SCRIPT).expect("the script");

    let line = script
        .lines()
        .find(|line| line.starts_with("readonly MARKER="))
        .expect("the script names its marker");

    assert_eq!(line, format!("readonly MARKER='{MARKER}'"));
}
