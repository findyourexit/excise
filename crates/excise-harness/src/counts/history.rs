//! The count history: one record per commit of `main`, on the orphan `bench-data` branch.
//!
//! # Layout
//!
//! A record is a `harness-counts` document in its own file,
//! `records/<os>/<first two hex digits of the commit>/<commit>.json`. One file per commit, rather
//! than one JSON Lines file per operating system, because
//!
//! * a pull request's comment finds the record of its base commit by name, without reading an
//!   ever-growing file,
//! * two writers never edit the same file, so a push that loses a race is a plain retry on the new
//!   tip, never a merge,
//! * every record is exactly one document, held to the same schema as a pull request's artifact,
//!   with no line framing, and
//! * a record is never edited: a commit that has one is not recorded again.
//!
//! The two-digit directory keeps a directory from growing past what a web listing shows.
//!
//! # Lookup
//!
//! History is written by a job that a busy `main` can leave behind (a push that is superseded
//! while another run is pending is never recorded), so a pull request's base commit may have no
//! record. [`find_base`] walks the base commit's first-parent ancestry, nearest first, and takes
//! the first commit that has one.

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    thread,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use thiserror::Error;

use crate::report::{CountsInvalid, Document, HarnessCounts};

use super::{
    artifact::{ArtifactError, read_untrusted},
    text::log_safe,
};

/// The directory of the branch that holds the records.
pub const RECORDS_DIR: &str = "records";

/// The name of the history branch.
pub const DEFAULT_BRANCH: &str = "bench-data";

/// How far back from a base commit a record is looked for: the base commit's own record, and
/// those of up to this many of its first-parent ancestors (so, at most one more commit than this
/// is searched).
pub const ANCESTOR_LIMIT: usize = 200;

/// The file that the first commit of the branch adds, and nothing after it changes.
pub const README: &str = "# bench-data\n\
\n\
The count history of `excise`, written only by the `Count history` workflow on every push to\n\
`main`, and read by the `Pull-request count comment` workflow.\n\
\n\
Each commit of `main` has at most one record, `records/<os>/<first two hex digits of the\n\
commit>/<commit>.json`: a `harness-counts` document, described by\n\
`crates/excise-harness/schemas/harness-counts.schema.json` on `main`. A record is never edited.\n\
Do not edit this branch by hand. See `docs/development.md` on `main`, \"Counts and count history\".\n";

/// The identity that signs the commits of the branch.
pub const COMMITTER_NAME: &str = "github-actions[bot]";
/// The address of that identity.
pub const COMMITTER_EMAIL: &str = "41898282+github-actions[bot]@users.noreply.github.com";

/// Whether `os` is a name that can be a directory of the branch.
fn valid_os(os: &str) -> bool {
    let mut characters = os.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_lowercase())
        && os.len() <= 16
        && characters.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Whether `text` is a full, lowercase hexadecimal commit.
#[must_use]
pub fn is_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `branch` is a branch name that is safe to put in a refspec.
fn valid_branch(branch: &str) -> bool {
    !branch.is_empty()
        && branch.len() <= 100
        && !branch.contains("..")
        && !branch.starts_with(['-', '/', '.'])
        && !branch.ends_with(['/', '.'])
        && branch
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._/-".contains(&byte))
}

/// The path of the record of `commit` for operating system `os`, relative to the root of the
/// branch and `/`-separated, or `None` where either is not a valid name.
#[must_use]
pub fn record_path(os: &str, commit: &str) -> Option<String> {
    (valid_os(os) && is_commit(commit))
        .then(|| format!("{RECORDS_DIR}/{os}/{}/{commit}.json", &commit[..2]))
}

/// A record that cannot be used, and why.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum RecordError {
    /// The record breaks the schema or cannot be read.
    #[error(transparent)]
    Invalid(#[from] ArtifactError),
    /// The record is a valid document that is not the record it is filed as.
    #[error("it is not the record of that commit and operating system: {0}")]
    Misfiled(&'static str),
}

/// Reads the record of `commit` for `os` below `history`, the root of a checkout of the branch.
///
/// A record is read as untrusted input like a pull request's artifact, and must be what it is
/// filed as: the document of that commit, for that operating system, of a commit rather than of
/// a pull request.
///
/// # Errors
///
/// Returns why a record that exists cannot be used. A record that does not exist is `Ok(None)`.
pub fn read_record(
    history: &Path,
    os: &str,
    commit: &str,
) -> Result<Option<HarnessCounts>, RecordError> {
    let Some(relative) = record_path(os, commit) else {
        return Err(RecordError::Misfiled("it has no valid name"));
    };
    let path = history.join(relative);
    match fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ArtifactError::Unreadable(log_safe(&error.to_string(), 160)).into());
        }
    }
    let document = read_untrusted(&path)?;
    if document.context.git_sha != commit {
        return Err(RecordError::Misfiled("it records another commit"));
    }
    if document.context.runner.os != os {
        return Err(RecordError::Misfiled(
            "it was taken on another operating system",
        ));
    }
    if document.context.pull_request.is_some() {
        return Err(RecordError::Misfiled("it was taken for a pull request"));
    }
    Ok(Some(document))
}

/// The record that a comparison is made against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRecord {
    /// The record.
    pub document: HarnessCounts,
    /// The commit it records.
    pub commit: String,
    /// How many commits before the base commit that is: 0 for the base commit itself.
    pub distance: usize,
}

/// A record that exists and was passed over, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    /// The commit the record is filed under.
    pub commit: String,
    /// Why it cannot be used.
    pub reason: String,
}

/// What a search of the history found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseSearch {
    /// The nearest record, if any commit searched has one.
    pub found: Option<BaseRecord>,
    /// How many commits were searched: all of them where nothing was found.
    pub searched: usize,
    /// The records that exist and cannot be used, which the search passed over.
    pub rejected: Vec<Rejected>,
}

/// Finds the record of the first of `ancestors` that has one.
///
/// `ancestors` is the base commit followed by its ancestors, nearest first (see [`ancestors`]).
/// A record that exists and cannot be used is passed over, and reported in the result, rather
/// than ending the search: the next ancestor's record is as good a base as it would have been.
#[must_use]
pub fn find_base(history: &Path, os: &str, ancestors: &[String]) -> BaseSearch {
    let mut rejected = Vec::new();
    for (distance, commit) in ancestors.iter().enumerate() {
        match read_record(history, os, commit) {
            Ok(Some(document)) => {
                return BaseSearch {
                    found: Some(BaseRecord {
                        document,
                        commit: commit.clone(),
                        distance,
                    }),
                    searched: distance + 1,
                    rejected,
                };
            }
            Ok(None) => {}
            Err(error) => rejected.push(Rejected {
                commit: commit.clone(),
                reason: error.to_string(),
            }),
        }
    }
    BaseSearch {
        found: None,
        searched: ancestors.len(),
        rejected,
    }
}

/// Something went wrong with git, or with writing the history.
#[derive(Debug, Error)]
pub enum HistoryError {
    /// A commit or a name is not one this module will give to git.
    #[error("{0}")]
    Refused(String),
    /// A record that this build writes must be a record of a commit.
    #[error("a record is of a commit on `main`, not of a pull request")]
    PullRequestRecord,
    /// The record is not a valid document.
    #[error("the record is not a valid document: {0}")]
    Invalid(#[from] CountsInvalid),
    /// The record could not be rendered.
    #[error("cannot render the record: {0}")]
    Json(#[from] serde_json::Error),
    /// git could not be run.
    #[error("cannot run git: {0}")]
    Spawn(io::Error),
    /// A git command failed.
    #[error("`git {command}` failed ({status}): {stderr}")]
    Git {
        /// The command, without its arguments from outside.
        command: &'static str,
        /// How it ended.
        status: String,
        /// What it said, made safe to print.
        stderr: String,
    },
    /// Every push was rejected.
    #[error("the push was rejected {attempts} times in a row: {stderr}")]
    PushRejected {
        /// How many pushes were tried.
        attempts: u32,
        /// What the last one said.
        stderr: String,
    },
    /// A file or directory could not be written.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: &'static str,
        /// The underlying error.
        source: io::Error,
    },
}

/// What git needs to authenticate to a remote over HTTPS.
///
/// The header holds a credential, so `Debug` never shows it.
#[derive(Clone, PartialEq, Eq)]
pub struct Auth {
    /// The URL the header applies to, for example `https://github.com/`.
    pub url_prefix: String,
    /// The value of the extra header: `AUTHORIZATION: basic <credentials>`.
    pub header: String,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Auth")
            .field("url_prefix", &self.url_prefix)
            .field("header", &"<redacted>")
            .finish()
    }
}

impl Auth {
    /// The header that authenticates `token` (a `GITHUB_TOKEN`) to `remote`, as
    /// `actions/checkout` sends it, or `None` where the remote is not an HTTPS URL.
    #[must_use]
    pub fn for_token(remote: &str, token: &str) -> Option<Self> {
        let rest = remote.strip_prefix("https://")?;
        let host = rest.split('/').next().filter(|host| !host.is_empty())?;
        Some(Self {
            url_prefix: format!("https://{host}/"),
            header: format!(
                "AUTHORIZATION: basic {}",
                STANDARD.encode(format!("x-access-token:{token}"))
            ),
        })
    }
}

/// The environment git runs in: no configuration but ours, no prompt, and no repository chosen
/// from the caller's environment.
fn git_command(dir: &Path, auth: Option<&Auth>) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    for inherited in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(inherited);
    }
    if let Some(auth) = auth {
        command
            .env("GIT_CONFIG_COUNT", "1")
            .env(
                "GIT_CONFIG_KEY_0",
                format!("http.{}.extraheader", auth.url_prefix),
            )
            .env("GIT_CONFIG_VALUE_0", &auth.header);
    }
    command
}

fn run(mut command: Command) -> Result<Output, HistoryError> {
    command.output().map_err(HistoryError::Spawn)
}

/// The error for a git command that did not succeed, with what it said made safe to print.
fn failure(output: &Output, command: &'static str) -> HistoryError {
    HistoryError::Git {
        command,
        status: output.status.to_string(),
        stderr: log_safe(String::from_utf8_lossy(&output.stderr).trim(), 400),
    }
}

fn succeeded(output: Output, command: &'static str) -> Result<Output, HistoryError> {
    if output.status.success() {
        Ok(output)
    } else {
        Err(failure(&output, command))
    }
}

/// The first-parent ancestry of `commit` in the repository at `repo`: the commit itself, then
/// its parent, then that commit's parent, and so on up to `distance` commits before it (so the
/// list has at most `distance + 1` commits).
///
/// First-parent is the history of `main` as a reader sees it: a commit that a merge brought in is
/// not on it, and `main` keeps a linear history anyway.
///
/// # Errors
///
/// Returns why `commit` is not one that git will be given, or why git could not walk from it
/// (for instance because the repository does not have it).
pub fn ancestors(repo: &Path, commit: &str, distance: usize) -> Result<Vec<String>, HistoryError> {
    if !is_commit(commit) {
        return Err(HistoryError::Refused(
            "the commit is not 40 lowercase hexadecimal digits".to_owned(),
        ));
    }
    let mut command = git_command(repo, None);
    command.args(["rev-list", "--first-parent"]);
    // The commit itself is listed too, so `distance` ancestors take one more.
    command.arg(format!("--max-count={}", distance.saturating_add(1)));
    command.arg(commit);
    let output = succeeded(run(command)?, "rev-list")?;
    let text = String::from_utf8_lossy(&output.stdout);
    let found: Vec<String> = text.lines().map(str::to_owned).collect();
    if found.iter().any(|line| !is_commit(line)) {
        return Err(HistoryError::Refused(
            "git listed something that is not a commit".to_owned(),
        ));
    }
    Ok(found)
}

/// How a record is to be appended.
#[derive(Debug, Clone)]
pub struct AppendOptions {
    /// Where the branch is: a URL, or a path.
    pub remote: String,
    /// The branch.
    pub branch: String,
    /// How to authenticate to the remote, if it needs it.
    pub auth: Option<Auth>,
    /// How many times the whole append is tried before it is given up: a push that loses a race
    /// is retried on the new tip. At least 1.
    pub attempts: u32,
    /// How long to wait before the second try. Each later try waits one more unit.
    pub retry_delay: Duration,
    /// An existing directory that the working copies of the branch are made in, and removed from.
    pub work_dir: PathBuf,
}

/// What appending did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Appended {
    /// The record is on the branch now.
    Recorded {
        /// Whether this created the branch.
        branch_created: bool,
    },
    /// The branch already has a record of that commit, which is left as it is.
    AlreadyRecorded,
}

/// Appends `record` to the history branch: one new file, in one new commit, on the tip of the
/// branch, which is created as an orphan, with a README, if the remote does not have it.
///
/// Nothing else on the branch changes. A push that is rejected because another writer got there
/// first starts again from the new tip, up to `options.attempts` times in all.
///
/// # Errors
///
/// Returns why the record or the options are not acceptable, why a git command failed, or that
/// every push was rejected.
pub fn append_record(
    options: &AppendOptions,
    record: &HarnessCounts,
) -> Result<Appended, HistoryError> {
    if !valid_branch(&options.branch) {
        return Err(HistoryError::Refused(format!(
            "`{}` is not a branch name this will push to",
            log_safe(&options.branch, 100)
        )));
    }
    if record.context.pull_request.is_some() {
        return Err(HistoryError::PullRequestRecord);
    }
    record.check()?;
    let commit = &record.context.git_sha;
    let os = &record.context.runner.os;
    let relative = record_path(os, commit).ok_or_else(|| {
        HistoryError::Refused("the record has no valid operating system or commit".to_owned())
    })?;
    let text = record.to_json_pretty()?;

    let attempts = options.attempts.max(1);
    let mut rejected = String::new();
    for attempt in 1..=attempts {
        match attempt_once(options, &relative, &text, commit, os)? {
            Attempt::Done(done) => return Ok(done),
            Attempt::Rejected(stderr) => rejected = stderr,
        }
        if attempt < attempts {
            thread::sleep(options.retry_delay * attempt);
        }
    }
    Err(HistoryError::PushRejected {
        attempts,
        stderr: rejected,
    })
}

enum Attempt {
    Done(Appended),
    Rejected(String),
}

fn io_error(context: &'static str) -> impl FnOnce(io::Error) -> HistoryError {
    move |source| HistoryError::Io { context, source }
}

/// A branch of a remote repository, and how to reach it.
struct Remote<'a> {
    url: &'a str,
    branch: &'a str,
    auth: Option<&'a Auth>,
}

impl Remote<'_> {
    fn branch_ref(&self) -> String {
        format!("refs/heads/{}", self.branch)
    }

    /// Runs `git` with `args` in `dir`.
    fn git(&self, dir: &Path, args: &[&str], name: &'static str) -> Result<Output, HistoryError> {
        let mut command = git_command(dir, self.auth);
        command.args(args);
        succeeded(run(command)?, name)
    }

    /// Makes the empty directory `dir` a working copy of the tip of the branch, or a repository
    /// whose next commit begins the branch where the remote does not have it. Returns whether
    /// the remote has the branch.
    fn clone_tip(&self, dir: &Path) -> Result<bool, HistoryError> {
        self.git(dir, &["init", "-q"], "init")?;
        let branch_ref = self.branch_ref();
        let mut listing = git_command(dir, self.auth);
        listing
            .args(["ls-remote", "--exit-code", "--heads"])
            .arg(self.url)
            .arg(&branch_ref);
        let listing = run(listing)?;
        // `--exit-code` makes "the branch does not exist" a status of its own (2), apart from a
        // remote that cannot be reached or refuses the credentials.
        let exists = match listing.status.code() {
            Some(0) => true,
            Some(2) => false,
            _ => return Err(failure(&listing, "ls-remote")),
        };
        if exists {
            let mut fetch = git_command(dir, self.auth);
            fetch
                .args(["fetch", "-q", "--depth=1"])
                .arg(self.url)
                .arg(&branch_ref);
            succeeded(run(fetch)?, "fetch")?;
            self.git(
                dir,
                &["checkout", "-q", "-B", self.branch, "FETCH_HEAD"],
                "checkout",
            )?;
        } else {
            self.git(dir, &["symbolic-ref", "HEAD", &branch_ref], "symbolic-ref")?;
        }
        Ok(exists)
    }
}

fn work_copy(work_dir: &Path) -> Result<tempfile::TempDir, HistoryError> {
    tempfile::Builder::new()
        .prefix("bench-data-")
        .tempdir_in(work_dir)
        .map_err(io_error(
            "cannot create a working copy of the history branch",
        ))
}

/// One try: a fresh working copy of the branch's tip, the record added, and a push.
fn attempt_once(
    options: &AppendOptions,
    relative: &str,
    text: &str,
    commit: &str,
    os: &str,
) -> Result<Attempt, HistoryError> {
    let work = work_copy(&options.work_dir)?;
    let dir = work.path();
    let remote = Remote {
        url: &options.remote,
        branch: &options.branch,
        auth: options.auth.as_ref(),
    };
    let exists = remote.clone_tip(dir)?;

    let path = dir.join(relative);
    if path.exists() {
        return Ok(Attempt::Done(Appended::AlreadyRecorded));
    }
    let parent = path.parent().unwrap_or(dir);
    fs::create_dir_all(parent).map_err(io_error("cannot create the record's directory"))?;
    fs::File::create_new(&path)
        .and_then(|mut file| file.write_all(text.as_bytes()))
        .map_err(io_error("cannot write the record"))?;
    remote.git(dir, &["add", "--", relative], "add")?;
    if !exists {
        fs::write(dir.join("README.md"), README).map_err(io_error("cannot write the README"))?;
        remote.git(dir, &["add", "--", "README.md"], "add")?;
    }
    let message = format!("bench-data: record {os} counts of {}", &commit[..12]);
    remote.git(
        dir,
        &[
            "-c",
            &format!("user.name={COMMITTER_NAME}"),
            "-c",
            &format!("user.email={COMMITTER_EMAIL}"),
            "commit",
            "-q",
            "--no-verify",
            "-m",
            &message,
        ],
        "commit",
    )?;

    let mut push = git_command(dir, remote.auth);
    push.args(["push", "-q"])
        .arg(remote.url)
        .arg(format!("HEAD:{}", remote.branch_ref()));
    let pushed = run(push)?;
    if pushed.status.success() {
        Ok(Attempt::Done(Appended::Recorded {
            branch_created: !exists,
        }))
    } else {
        Ok(Attempt::Rejected(log_safe(
            String::from_utf8_lossy(&pushed.stderr).trim(),
            400,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::counts::test_support::{case, commit, for_pull_request, record, suite};

    fn recorded(commit: &str, store: u64) -> HarnessCounts {
        record(commit, suite(store))
    }

    /// Writes `document` where the record of its commit belongs below `history`.
    fn file(history: &Path, document: &HarnessCounts) -> PathBuf {
        let relative = record_path(&document.context.runner.os, &document.context.git_sha)
            .expect("a valid record name");
        let path = history.join(relative);
        fs::create_dir_all(path.parent().expect("a directory")).expect("directories");
        fs::write(&path, document.to_json_pretty().expect("renders")).expect("a record");
        path
    }

    #[test]
    fn a_record_is_filed_by_operating_system_and_the_first_two_digits_of_its_commit() {
        assert_eq!(
            record_path("linux", &commit(0xab)),
            Some(format!("records/linux/ab/{}.json", commit(0xab)))
        );
        assert_eq!(
            record_path("macos", &commit(0x07)),
            Some(format!("records/macos/07/{}.json", commit(0x07)))
        );
    }

    #[test]
    fn a_name_that_could_leave_its_directory_is_not_a_record_name() {
        let good = commit(0xab);
        for os in [
            "",
            "../x",
            "Linux",
            "linux/x",
            "li nux",
            "1linux",
            &"a".repeat(17),
            "lin\u{e9}x",
        ] {
            assert_eq!(record_path(os, &good), None, "{os:?}");
        }
        for bad in [
            "",
            "abc",
            &good.to_uppercase(),
            &good[..39],
            &format!("{good}0"),
            &format!("../{}", &good[..37]),
            &"g".repeat(40),
        ] {
            assert_eq!(record_path("linux", bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_a_branch_name_that_is_safe_in_a_refspec_is_accepted() {
        for good in ["bench-data", "bench-data-probe", "probe/b3_1", "v1.2"] {
            assert!(valid_branch(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "--force",
            "a..b",
            "a b",
            "a;b",
            "a:b",
            "a^b",
            ".hidden",
            "/x",
            "x/",
            "x.",
            "a\\b",
            "a\nb",
            "a~1",
            &"x".repeat(101),
        ] {
            assert!(!valid_branch(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_authentication_header_is_the_one_actions_checkout_sends_and_is_never_debug_printed() {
        let auth =
            Auth::for_token("https://github.com/findyourexit/excise", "s3cr3t").expect("https");

        assert_eq!(auth.url_prefix, "https://github.com/");
        assert_eq!(
            auth.header,
            format!(
                "AUTHORIZATION: basic {}",
                STANDARD.encode("x-access-token:s3cr3t")
            )
        );
        let debug = format!("{auth:?} {:?}", Some(&auth));
        assert!(
            !debug.contains("s3cr3t") && !debug.contains(&STANDARD.encode("x-access-token:s3cr3t")),
            "{debug}"
        );
        assert!(debug.contains("redacted"), "{debug}");
        for not_https in [
            "/tmp/remote.git",
            "http://github.com/o/r",
            "git@github.com:o/r.git",
            "https://",
            "file:///tmp/r",
        ] {
            assert_eq!(Auth::for_token(not_https, "s3cr3t"), None, "{not_https}");
        }
    }

    #[test]
    fn a_missing_record_is_none_and_a_found_one_is_the_document() {
        let history = tempfile::tempdir().expect("a directory");
        let document = recorded(&commit(0x11), 350);
        file(history.path(), &document);

        assert_eq!(
            read_record(history.path(), "linux", &commit(0x11)),
            Ok(Some(document))
        );
        assert_eq!(
            read_record(history.path(), "linux", &commit(0x22)),
            Ok(None)
        );
        assert_eq!(
            read_record(history.path(), "macos", &commit(0x11)),
            Ok(None),
            "one series per operating system"
        );
    }

    #[test]
    fn a_record_that_is_not_what_it_is_filed_as_is_refused() {
        let history = tempfile::tempdir().expect("a directory");
        // Filed under 0x11 but recording 0x22; filed under linux but taken on macos; taken for a
        // pull request.
        let mut elsewhere = recorded(&commit(0x22), 350);
        let path = file(history.path(), &elsewhere);
        fs::create_dir_all(history.path().join("records/linux/11")).expect("dir");
        fs::rename(
            &path,
            history
                .path()
                .join(record_path("linux", &commit(0x11)).expect("name")),
        )
        .expect("move");
        elsewhere = recorded(&commit(0x33), 350);
        elsewhere.context.runner.os = "macos".to_owned();
        let wrong_os = history
            .path()
            .join(record_path("linux", &commit(0x33)).expect("name"));
        fs::create_dir_all(wrong_os.parent().expect("dir")).expect("dir");
        fs::write(&wrong_os, elsewhere.to_json_pretty().expect("renders")).expect("write");
        let pull_request = for_pull_request(
            recorded(&commit(0x44), 350),
            5,
            &commit(0xb0),
            &commit(0xaa),
        );
        let wrong_kind = history
            .path()
            .join(record_path("linux", &commit(0x44)).expect("name"));
        fs::create_dir_all(wrong_kind.parent().expect("dir")).expect("dir");
        fs::write(&wrong_kind, pull_request.to_json_pretty().expect("renders")).expect("write");

        for (commit, why) in [
            (commit(0x11), "it records another commit"),
            (commit(0x33), "it was taken on another operating system"),
            (commit(0x44), "it was taken for a pull request"),
        ] {
            assert_eq!(
                read_record(history.path(), "linux", &commit),
                Err(RecordError::Misfiled(why)),
                "{commit}"
            );
        }
    }

    #[test]
    fn a_record_that_breaks_the_schema_is_refused_like_any_untrusted_document() {
        let history = tempfile::tempdir().expect("a directory");
        let path = file(history.path(), &recorded(&commit(0x11), 350));
        fs::write(&path, "{\"document_kind\": \"harness-counts\"}").expect("overwrite");

        let error = read_record(history.path(), "linux", &commit(0x11)).expect_err("invalid");

        assert!(
            matches!(error, RecordError::Invalid(ArtifactError::Schema(_))),
            "{error:?}"
        );
    }

    fn chain(count: u8) -> Vec<String> {
        (1..=count).map(commit).collect()
    }

    #[test]
    fn the_base_commits_own_record_is_the_nearest() {
        let history = tempfile::tempdir().expect("a directory");
        for byte in [1, 2, 3] {
            file(history.path(), &recorded(&commit(byte), 350));
        }

        let search = find_base(history.path(), "linux", &chain(5));

        let found = search.found.expect("a record");
        assert_eq!(
            (found.commit, found.distance, search.searched),
            (commit(1), 0, 1)
        );
    }

    #[test]
    fn a_base_commit_with_no_record_falls_back_to_its_nearest_ancestor_that_has_one() {
        let history = tempfile::tempdir().expect("a directory");
        // The base (1) and its parent (2) were never recorded; 3 and 5 were.
        for byte in [3, 5] {
            file(history.path(), &recorded(&commit(byte), 350));
        }

        let search = find_base(history.path(), "linux", &chain(5));

        let found = search.found.expect("a record");
        assert_eq!(found.commit, commit(3), "the nearest, not the oldest");
        assert_eq!(found.distance, 2);
        assert_eq!(found.document.context.git_sha, commit(3));
        assert_eq!(search.searched, 3);
    }

    #[test]
    fn no_record_anywhere_searches_every_ancestor_and_finds_nothing() {
        let history = tempfile::tempdir().expect("a directory");
        file(history.path(), &recorded(&commit(9), 350));

        let search = find_base(history.path(), "linux", &chain(5));

        assert_eq!(
            (search.found, search.searched, search.rejected),
            (None, 5, Vec::new())
        );
        let none = find_base(history.path(), "linux", &[]);
        assert_eq!((none.found, none.searched), (None, 0));
        let other_os = find_base(history.path(), "macos", &[commit(9)]);
        assert_eq!(
            other_os.found, None,
            "a Linux record is no base for a macOS count"
        );
    }

    #[test]
    fn a_record_that_cannot_be_used_is_passed_over_and_reported_not_fatal() {
        let history = tempfile::tempdir().expect("a directory");
        let broken = file(history.path(), &recorded(&commit(1), 350));
        fs::write(&broken, "not json").expect("overwrite");
        file(history.path(), &recorded(&commit(2), 350));

        let search = find_base(history.path(), "linux", &chain(3));

        let found = search.found.expect("the next record is used");
        assert_eq!((found.commit, found.distance), (commit(2), 1));
        assert_eq!(search.rejected.len(), 1);
        assert_eq!(search.rejected[0].commit, commit(1));
        assert!(
            search.rejected[0].reason.contains("not JSON"),
            "{:?}",
            search.rejected[0]
        );
    }

    #[test]
    fn a_record_of_another_fixture_set_is_still_a_base_the_comparison_judges_case_by_case() {
        let history = tempfile::tempdir().expect("a directory");
        file(
            history.path(),
            &record(
                &commit(1),
                vec![case(
                    "only-then",
                    crate::counts::test_support::HASH,
                    &[("entries", 1)],
                )],
            ),
        );

        assert!(
            find_base(history.path(), "linux", &chain(1))
                .found
                .is_some()
        );
    }

    /// The tests that run `git`, which a development machine and every hosted runner has and a
    /// build sandbox may not.
    #[cfg(unix)]
    mod git {
        use std::os::unix::fs::PermissionsExt as _;

        use super::*;

        /// Runs git in `dir` as the module does, and returns what it printed.
        fn git(dir: &Path, args: &[&str]) -> String {
            let output = git_command(dir, None)
                .args(args)
                .output()
                .expect("git runs");
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        /// Makes an empty commit in `dir` and returns its id.
        fn commit_empty(dir: &Path, message: &str) -> String {
            git(
                dir,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.test",
                    "commit",
                    "-q",
                    "--allow-empty",
                    "-m",
                    message,
                ],
            );
            git(dir, &["rev-parse", "HEAD"])
        }

        fn bare_remote() -> tempfile::TempDir {
            let remote = tempfile::tempdir().expect("a directory");
            git(remote.path(), &["init", "-q", "--bare", "-b", "main"]);
            remote
        }

        fn options(remote: &Path, work: &Path) -> AppendOptions {
            AppendOptions {
                remote: remote.to_str().expect("a UTF-8 path").to_owned(),
                branch: DEFAULT_BRANCH.to_owned(),
                auth: None,
                attempts: 3,
                retry_delay: Duration::ZERO,
                work_dir: work.to_path_buf(),
            }
        }

        /// Makes the remote refuse the first `times` pushes, as it does when another writer got
        /// there first.
        fn refuse_pushes(remote: &Path, times: u32) {
            let counter = remote.join("refused");
            let hook = remote.join("hooks/pre-receive");
            fs::write(
                &hook,
                format!(
                    "#!/bin/sh\nn=$(cat '{c}' 2>/dev/null || echo 0)\nif [ \"$n\" -lt {times} ]; then\n  echo $((n + 1)) > '{c}'\n  echo 'simulated: another writer got there first' >&2\n  exit 1\nfi\n",
                    c = counter.display()
                ),
            )
            .expect("a hook");
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("executable");
        }

        fn files_on(remote: &Path) -> Vec<String> {
            git(remote, &["ls-tree", "-r", "--name-only", DEFAULT_BRANCH])
                .lines()
                .map(str::to_owned)
                .collect()
        }

        fn commits_on(remote: &Path) -> usize {
            git(remote, &["rev-list", "--count", DEFAULT_BRANCH])
                .parse()
                .expect("a count")
        }

        #[test]
        fn the_first_record_creates_the_branch_as_an_orphan_with_a_readme_and_the_record() {
            let (remote, work) = (bare_remote(), tempfile::tempdir().expect("a directory"));
            let document = recorded(&commit(0x11), 350);

            let appended = append_record(&options(remote.path(), work.path()), &document);

            assert_eq!(
                appended.expect("appended"),
                Appended::Recorded {
                    branch_created: true
                }
            );
            let path = record_path("linux", &commit(0x11)).expect("name");
            assert_eq!(
                files_on(remote.path()),
                ["README.md".to_owned(), path.clone()]
            );
            assert_eq!(commits_on(remote.path()), 1);
            assert_eq!(
                git(
                    remote.path(),
                    &["rev-list", "--max-parents=0", DEFAULT_BRANCH]
                ),
                git(remote.path(), &["rev-parse", DEFAULT_BRANCH]),
                "the branch has no ancestry of its own beyond one root commit"
            );
            assert_eq!(
                git(
                    remote.path(),
                    &["show", &format!("{DEFAULT_BRANCH}:{path}")]
                ),
                document.to_json_pretty().expect("renders").trim_end(),
                "the record is the document in its canonical form"
            );
            assert_eq!(
                git(
                    remote.path(),
                    &["show", &format!("{DEFAULT_BRANCH}:README.md")]
                ),
                README.trim_end()
            );
            assert_eq!(
                git(
                    remote.path(),
                    &["log", "-1", "--format=%an <%ae>", DEFAULT_BRANCH]
                ),
                format!("{COMMITTER_NAME} <{COMMITTER_EMAIL}>")
            );
        }

        #[test]
        fn a_second_record_adds_one_file_and_changes_nothing_else_on_the_branch() {
            let (remote, work) = (bare_remote(), tempfile::tempdir().expect("a directory"));
            let options = options(remote.path(), work.path());
            append_record(&options, &recorded(&commit(0x11), 350)).expect("the first");
            let readme = git(
                remote.path(),
                &["rev-parse", &format!("{DEFAULT_BRANCH}:README.md")],
            );
            let first = git(
                remote.path(),
                &[
                    "rev-parse",
                    &format!(
                        "{DEFAULT_BRANCH}:{}",
                        record_path("linux", &commit(0x11)).expect("name")
                    ),
                ],
            );

            let appended = append_record(&options, &recorded(&commit(0x22), 360));

            assert_eq!(
                appended.expect("appended"),
                Appended::Recorded {
                    branch_created: false
                }
            );
            assert_eq!(commits_on(remote.path()), 2);
            assert_eq!(
                files_on(remote.path()).len(),
                3,
                "the README and two records"
            );
            assert_eq!(
                git(
                    remote.path(),
                    &["rev-parse", &format!("{DEFAULT_BRANCH}:README.md")]
                ),
                readme
            );
            assert_eq!(
                git(
                    remote.path(),
                    &[
                        "rev-parse",
                        &format!(
                            "{DEFAULT_BRANCH}:{}",
                            record_path("linux", &commit(0x11)).expect("name")
                        )
                    ]
                ),
                first,
                "an earlier record is never rewritten"
            );
            assert_eq!(
                git(
                    remote.path(),
                    &[
                        "diff",
                        "--name-status",
                        &format!("{DEFAULT_BRANCH}~1"),
                        DEFAULT_BRANCH
                    ]
                ),
                format!("A\t{}", record_path("linux", &commit(0x22)).expect("name")),
                "the whole change of the second commit is the new record"
            );
        }

        #[test]
        fn a_commit_that_has_a_record_is_not_recorded_again_and_the_branch_does_not_move() {
            let (remote, work) = (bare_remote(), tempfile::tempdir().expect("a directory"));
            let options = options(remote.path(), work.path());
            append_record(&options, &recorded(&commit(0x11), 350)).expect("the first");
            let tip = git(remote.path(), &["rev-parse", DEFAULT_BRANCH]);

            let again = append_record(&options, &recorded(&commit(0x11), 999));

            assert_eq!(again.expect("appended"), Appended::AlreadyRecorded);
            assert_eq!(git(remote.path(), &["rev-parse", DEFAULT_BRANCH]), tip);
        }

        #[test]
        fn a_pull_requests_counts_and_a_bad_branch_are_refused_before_anything_is_pushed() {
            let (remote, work) = (bare_remote(), tempfile::tempdir().expect("a directory"));
            let pull_request = for_pull_request(
                recorded(&commit(0x11), 350),
                5,
                &commit(0xb0),
                &commit(0xaa),
            );

            let refused = append_record(&options(remote.path(), work.path()), &pull_request);
            assert!(
                matches!(refused, Err(HistoryError::PullRequestRecord)),
                "{refused:?}"
            );
            let mut bad_branch = options(remote.path(), work.path());
            bad_branch.branch = "a;b".to_owned();
            let refused = append_record(&bad_branch, &recorded(&commit(0x11), 350));
            assert!(
                matches!(refused, Err(HistoryError::Refused(_))),
                "{refused:?}"
            );

            assert_eq!(
                git(remote.path(), &["branch", "--list"]),
                "",
                "nothing was pushed"
            );
        }

        #[test]
        fn a_push_that_loses_a_race_is_retried_on_the_new_tip_and_lands() {
            let (remote, work) = (bare_remote(), tempfile::tempdir().expect("a directory"));
            let options = options(remote.path(), work.path());
            append_record(&options, &recorded(&commit(0x11), 350)).expect("the first");
            refuse_pushes(remote.path(), 2);

            let appended = append_record(&options, &recorded(&commit(0x22), 360));

            assert_eq!(
                appended.expect("lands on the third try"),
                Appended::Recorded {
                    branch_created: false
                }
            );
            assert_eq!(
                commits_on(remote.path()),
                2,
                "the refused tries left nothing behind"
            );
            assert_eq!(files_on(remote.path()).len(), 3);
        }

        #[test]
        fn a_remote_that_always_refuses_stops_after_the_bound_and_says_so() {
            let (remote, work) = (bare_remote(), tempfile::tempdir().expect("a directory"));
            let options = options(remote.path(), work.path());
            append_record(&options, &recorded(&commit(0x11), 350)).expect("the first");
            refuse_pushes(remote.path(), 1_000);

            let appended = append_record(&options, &recorded(&commit(0x22), 360));

            match appended {
                Err(HistoryError::PushRejected { attempts, stderr }) => {
                    assert_eq!(attempts, 3, "bounded by the option");
                    assert!(stderr.contains("simulated"), "{stderr}");
                }
                other => panic!("expected the push to be given up, got {other:?}"),
            }
            assert_eq!(commits_on(remote.path()), 1);
            assert_eq!(
                fs::read_to_string(remote.path().join("refused"))
                    .expect("counter")
                    .trim(),
                "3"
            );
        }

        #[test]
        fn a_remote_that_cannot_be_reached_is_an_error_and_not_a_missing_branch() {
            let work = tempfile::tempdir().expect("a directory");
            let mut unreachable = options(Path::new("/definitely/not/a/remote"), work.path());
            unreachable.attempts = 1;

            let appended = append_record(&unreachable, &recorded(&commit(0x11), 350));

            assert!(
                matches!(
                    appended,
                    Err(HistoryError::Git {
                        command: "ls-remote",
                        ..
                    })
                ),
                "an unreachable remote must not be taken for a branch that does not exist: {appended:?}"
            );
        }

        #[test]
        fn what_is_recorded_is_found_in_a_plain_checkout_of_the_branch() {
            let (remote, work) = (bare_remote(), tempfile::tempdir().expect("a directory"));
            append_record(
                &options(remote.path(), work.path()),
                &recorded(&commit(0x11), 350),
            )
            .expect("recorded");

            // What the comment workflow does: a working copy of the branch, made by git alone
            // (the workflow's own checkout has every branch), searched for the base commit's
            // record, which here is one commit further back than the commit asked about.
            let checkout = work.path().join("checkout");
            git(
                work.path(),
                &[
                    "clone",
                    "-q",
                    "--branch",
                    DEFAULT_BRANCH,
                    remote.path().to_str().expect("a UTF-8 path"),
                    checkout.to_str().expect("a UTF-8 path"),
                ],
            );
            let search = find_base(&checkout, "linux", &[commit(0x22), commit(0x11)]);

            let found = search.found.expect("the record the writer filed is found");
            assert_eq!(found.commit, commit(0x11));
            assert_eq!(found.distance, 1);
            assert!(search.rejected.is_empty(), "{:?}", search.rejected);
        }

        /// A repository at `dir` with a linear history of `count` commits, made in one go by
        /// `git fast-import`. Returns their ids, newest first.
        fn linear_history(dir: &Path, count: usize) -> Vec<String> {
            use std::{fmt::Write as _, io::Write as _};

            git(dir, &["init", "-q", "-b", "main"]);
            let mut stream = String::new();
            for index in 0..count {
                let message = format!("commit {index}");
                let _ = write!(
                    stream,
                    "commit refs/heads/main\ncommitter t <t@example.test> {} +0000\ndata {}\n{message}\n\n",
                    1_700_000_000 + index,
                    message.len()
                );
            }
            let mut import = git_command(dir, None)
                .arg("fast-import")
                .arg("--quiet")
                .stdin(Stdio::piped())
                .spawn()
                .expect("git fast-import runs");
            import
                .stdin
                .take()
                .expect("its input")
                .write_all(stream.as_bytes())
                .expect("the history is written");
            assert!(
                import.wait().expect("it ends").success(),
                "fast-import failed"
            );
            git(dir, &["rev-list", "--first-parent", "main"])
                .lines()
                .map(str::to_owned)
                .collect()
        }

        /// The documented reach of the search: the base commit and up to `ANCESTOR_LIMIT` of its
        /// ancestors, so a record exactly that many commits back is found and one more is not.
        #[test]
        fn a_record_exactly_the_limit_back_is_found_and_one_commit_further_is_not() {
            let repo = tempfile::tempdir().expect("a directory");
            let history = linear_history(repo.path(), ANCESTOR_LIMIT + 3);
            assert_eq!(history.len(), ANCESTOR_LIMIT + 3);
            let chain = ancestors(repo.path(), &history[0], ANCESTOR_LIMIT).expect("a chain");
            assert_eq!(
                chain.last(),
                Some(&history[ANCESTOR_LIMIT]),
                "the chain ends at the farthest commit the search is documented to reach"
            );

            let at_the_limit = tempfile::tempdir().expect("a directory");
            file(
                at_the_limit.path(),
                &recorded(&history[ANCESTOR_LIMIT], 350),
            );
            let found = find_base(at_the_limit.path(), "linux", &chain);
            let record = found.found.expect("the record at the limit is found");
            assert_eq!(
                (record.commit.as_str(), record.distance),
                (history[ANCESTOR_LIMIT].as_str(), ANCESTOR_LIMIT)
            );

            let beyond = tempfile::tempdir().expect("a directory");
            file(beyond.path(), &recorded(&history[ANCESTOR_LIMIT + 1], 350));
            let missed = find_base(beyond.path(), "linux", &chain);
            assert_eq!(missed.found, None, "one commit further is out of reach");
            assert_eq!(
                missed.searched,
                ANCESTOR_LIMIT + 1,
                "the base and its {ANCESTOR_LIMIT} ancestors were searched"
            );
        }

        #[test]
        fn the_ancestry_of_a_commit_is_its_first_parent_chain_nearest_first() {
            let repo = tempfile::tempdir().expect("a directory");
            let dir = repo.path();
            git(dir, &["init", "-q", "-b", "main"]);
            let first = commit_empty(dir, "first");
            git(dir, &["checkout", "-q", "-b", "side"]);
            let side = commit_empty(dir, "side");
            git(dir, &["checkout", "-q", "main"]);
            let second = commit_empty(dir, "second");
            git(
                dir,
                &[
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@example.test",
                    "merge",
                    "-q",
                    "--no-ff",
                    "-m",
                    "merge",
                    "side",
                ],
            );
            let merge = git(dir, &["rev-parse", "HEAD"]);
            let third = commit_empty(dir, "third");

            let found = ancestors(dir, &third, 100).expect("a chain");

            assert_eq!(
                found,
                [third.clone(), merge.clone(), second, first],
                "{side} is not on main"
            );
            assert!(
                !found.contains(&side),
                "a commit a merge brought in is not first-parent"
            );
            assert_eq!(
                ancestors(dir, &third, 1).expect("a chain"),
                [third.clone(), merge],
                "one ancestor beyond the commit itself"
            );
            assert_eq!(
                ancestors(dir, &third, 0).expect("a chain"),
                [third],
                "no distance is the commit alone"
            );
        }

        #[test]
        fn a_commit_that_is_not_in_the_repository_or_not_a_commit_name_has_no_ancestry() {
            let repo = tempfile::tempdir().expect("a directory");
            git(repo.path(), &["init", "-q", "-b", "main"]);
            commit_empty(repo.path(), "only");

            let unknown = ancestors(repo.path(), &commit(0xab), 10);
            assert!(
                matches!(
                    unknown,
                    Err(HistoryError::Git {
                        command: "rev-list",
                        ..
                    })
                ),
                "{unknown:?}"
            );
            // A name that is not 40 hexadecimal digits never reaches git: the directory need not
            // even exist.
            for not_a_commit in [
                "main",
                "HEAD",
                "--all",
                "-n1",
                &commit(0xab).to_uppercase(),
                "abc",
            ] {
                let refused =
                    ancestors(Path::new("/definitely/not/a/repository"), not_a_commit, 10);
                assert!(
                    matches!(refused, Err(HistoryError::Refused(_))),
                    "{not_a_commit}: {refused:?}"
                );
            }
        }
    }
}
