//! The script that checks a pull request's counts artifact before it is downloaded, run against a
//! stand-in for `gh`.
//!
//! The artifact is made by the pull request's own workflow run, so its size is the author's
//! choice, and the download unpacks all of it onto the runner before anything can read it. The
//! script asks GitHub what the run uploaded and refuses anything but one small `pr-counts`. These
//! tests run it with a `gh` that serves canned listings and records every request, so that what
//! it accepts, what it refuses, and what it asks for are checked without a network.
#![cfg(unix)]

use std::{
    fs,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU32, Ordering},
};

use excise_harness::counts::artifact::MAX_DOCUMENT_BYTES;

const SCRIPT: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../.github/scripts/check-count-artifact.sh"
);

/// A `gh` that answers only the request the script is meant to make, and says so loudly for any
/// other: anything unexpected exits non-zero.
const FAKE_GH: &str = r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$FAKE_DIR/calls"
[[ "$1" == api ]] || { echo "unexpected gh command: $*" >&2; exit 64; }
case "$2" in
  "repos/o/r/actions/runs/9/artifacts") cat "$FAKE_DIR/artifacts.json" ;;
  *) echo "unexpected request: $*" >&2; exit 65 ;;
esac
"#;

/// A directory for one test: the stand-in `gh` and the listing it serves.
struct Scene(PathBuf);

impl Scene {
    fn new(listing: &str) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!(
            "check-count-artifact-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("bin")).expect("a directory");
        fs::write(dir.join("bin/gh"), FAKE_GH).expect("the stand-in gh");
        fs::set_permissions(dir.join("bin/gh"), fs::Permissions::from_mode(0o755))
            .expect("executable");
        fs::write(dir.join("artifacts.json"), listing).expect("a listing");
        Self(dir)
    }

    /// Runs the script with the usual inputs, `overrides` replacing or adding to them.
    fn run(&self, overrides: &[(&str, &str)]) -> Output {
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
            .env("RUN_ID", "9");
        for (name, value) in overrides {
            command.env(name, value);
        }
        command.output().expect("bash runs the script")
    }

    /// The requests the script made, one per line.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.0.join("calls"))
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }
}

impl Drop for Scene {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
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

/// One artifact of a run's listing.
fn artifact(name: &str, size: &str, expired: bool) -> String {
    format!(
        r#"{{"id": 1, "name": {}, "size_in_bytes": {size}, "expired": {expired}}}"#,
        serde_json::to_string(name).expect("a string")
    )
}

/// A run's listing: `total` artifacts, of which `artifacts` are on this page.
fn listing(total: usize, artifacts: &[String]) -> String {
    format!(
        r#"{{"total_count": {total}, "artifacts": [{}]}}"#,
        artifacts.join(", ")
    )
}

fn one(name: &str, size: u64) -> String {
    listing(1, &[artifact(name, &size.to_string(), false)])
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn one_artifact_named_pr_counts_within_the_bound_may_be_downloaded() {
    needs_jq();
    let scene = Scene::new(&one("pr-counts", 1_234));

    let output = scene.run(&[]);

    assert!(output.status.success(), "{}", said(&output));
    assert!(
        said(&output).contains("may be downloaded"),
        "{}",
        said(&output)
    );
    assert_eq!(
        scene.calls(),
        ["api repos/o/r/actions/runs/9/artifacts"],
        "the listing is all that is asked for"
    );
}

#[test]
fn the_size_bound_is_exact() {
    needs_jq();
    let at_the_bound = Scene::new(&one("pr-counts", 65_536));
    let one_over = Scene::new(&one("pr-counts", 65_537));

    assert!(at_the_bound.run(&[]).status.success());
    let refused = one_over.run(&[]);
    assert!(!refused.status.success());
    assert!(
        said(&refused).contains("larger than 65536 bytes"),
        "{}",
        said(&refused)
    );
}

/// The bound that the script states is the one that the reader of the document holds the
/// document to, so the two cannot drift apart unnoticed.
#[test]
fn the_bound_of_the_script_is_the_bound_of_the_reader() {
    let script = fs::read_to_string(SCRIPT).expect("the script");

    let line = script
        .lines()
        .find(|line| line.starts_with("readonly MAX_BYTES="))
        .expect("the script states its bound");

    assert_eq!(line, format!("readonly MAX_BYTES={MAX_DOCUMENT_BYTES}"));
}

#[test]
fn any_other_number_of_artifacts_is_refused() {
    needs_jq();
    let listings = [
        ("none", listing(0, &[])),
        (
            "two",
            listing(
                2,
                &[
                    artifact("pr-counts", "100", false),
                    artifact("pr-counts", "100", false),
                ],
            ),
        ),
        (
            "one more than the page that was listed",
            listing(1_000, &[artifact("pr-counts", "100", false)]),
        ),
        (
            "another beside it",
            listing(
                2,
                &[
                    artifact("pr-counts", "100", false),
                    artifact("other", "100", false),
                ],
            ),
        ),
    ];
    for (what, text) in listings {
        let scene = Scene::new(&text);

        let output = scene.run(&[]);

        assert!(!output.status.success(), "{what} was accepted");
        assert!(
            said(&output).contains("exactly one, named pr-counts, is expected"),
            "{what}: {}",
            said(&output)
        );
    }
}

#[test]
fn an_artifact_with_another_name_is_refused_and_its_name_is_never_printed_or_run() {
    needs_jq();
    let injected = Path::new(env!("CARGO_TARGET_TMPDIR")).join("named-pwned");
    let _ = fs::remove_file(&injected);
    let hostile = format!("pr-counts$(touch {})", injected.display());
    for name in [
        "pr-counts-2",
        "PR-COUNTS",
        "pr-counts ",
        "counts",
        "",
        hostile.as_str(),
    ] {
        let scene = Scene::new(&one(name, 100));

        let output = scene.run(&[]);

        assert!(!output.status.success(), "{name:?} was accepted");
        assert_eq!(
            said(&output),
            "check-count-artifact: the one artifact of the run is not named pr-counts\n",
            "the message is the same whatever the name is: a name is only compared"
        );
    }
    assert!(!injected.exists(), "a name was run as a command");
}

#[test]
fn an_expired_artifact_or_one_without_a_size_is_refused() {
    needs_jq();
    let listings = [
        ("expired", listing(1, &[artifact("pr-counts", "100", true)])),
        (
            "a size that is not a number",
            listing(1, &[artifact("pr-counts", "\"100\"", false)]),
        ),
        (
            "no size",
            listing(
                1,
                &[r#"{"id": 1, "name": "pr-counts", "expired": false}"#.to_owned()],
            ),
        ),
    ];
    for (what, text) in listings {
        let scene = Scene::new(&text);

        assert!(!scene.run(&[]).status.success(), "{what} was accepted");
    }
}

#[test]
fn a_listing_that_cannot_be_read_is_refused() {
    needs_jq();
    for (what, text) in [
        ("not JSON", "<html>"),
        ("an array", "[]"),
        ("an error from the API", r#"{"message": "Not Found"}"#),
    ] {
        let scene = Scene::new(text);

        assert!(!scene.run(&[]).status.success(), "{what} was accepted");
    }
    // `gh` itself failing, as it does for a run that is not there, fails the step too.
    let scene = Scene::new("");
    fs::remove_file(scene.0.join("artifacts.json")).expect("no listing");
    assert!(!scene.run(&[]).status.success());
}

#[test]
fn every_input_is_checked_before_anything_is_asked_of_github() {
    needs_jq();
    let injected = Path::new(env!("CARGO_TARGET_TMPDIR")).join("run-pwned");
    let _ = fs::remove_file(&injected);
    let cases: Vec<(&str, String)> = vec![
        ("RUN_ID", String::new()),
        ("RUN_ID", "0".to_owned()),
        ("RUN_ID", "09".to_owned()),
        ("RUN_ID", "-9".to_owned()),
        ("RUN_ID", "9 9".to_owned()),
        ("RUN_ID", "9;true".to_owned()),
        ("RUN_ID", "../9".to_owned()),
        ("RUN_ID", "9\n9".to_owned()),
        ("RUN_ID", "12345678901234567890".to_owned()),
        ("RUN_ID", format!("9$(touch {})", injected.display())),
        ("REPOSITORY", String::new()),
        ("REPOSITORY", "o".to_owned()),
        ("REPOSITORY", "o/r/extra".to_owned()),
        ("REPOSITORY", "o/r;true".to_owned()),
        ("REPOSITORY", "../x/y".to_owned()),
        ("REPOSITORY", format!("o/r$(touch {})", injected.display())),
    ];
    for (name, value) in cases {
        let scene = Scene::new(&one("pr-counts", 100));

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
