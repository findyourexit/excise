//! `cargo xtask soak` as a person's shell runs it once cargo has started it: the real `xtask`
//! binary, with the release build replaced by a script and `excise` by a stand-in that leaves a
//! marker when it is started.
//!
//! The seam is `CARGO`, the program the command builds with. Cargo sets it, to itself, for
//! `cargo xtask`, so a shell cannot make the real command use another; these tests set it to a
//! script that "builds" as cargo does as far as the command can tell: it records its arguments and
//! what it was given (the cache-cleaning setting, the temporary directory, cargo's home directory,
//! the compiler wrappers), installs the stand-in where it says it did, and reports the file in a
//! `compiler-artifact` message. `EXCISE_E2E_BINARY`, which the other commands honor, is set to a
//! decoy that must never run, and so is the file at the path a build that is not told better
//! would guess. A script stands in for `git` too, first on the command's `PATH`, and records that
//! it was run, where, and with which variables of its own.
//!
//! What these tests start is the built `xtask` binary, directly (`CARGO_BIN_EXE_xtask`), and what
//! they show holds from the moment it runs. `cargo xtask` is an alias of `cargo run --locked
//! --package xtask --`, so a real invocation has cargo build and start xtask first, before it can
//! read a terminal or a prompt; no test here can show that cargo starts nothing, and the
//! documents say only what holds once xtask runs (the alias they describe is pinned below).
//! Without a terminal, with a mistyped path at the prompt, with an output directory that cannot
//! take a run (a symbolic link, a file, a `latest` that no run made), and with a root that holds a
//! place the build writes in or the scratch directory, xtask refuses before it builds or starts
//! anything, `git` included. At a terminal, with the right path, it builds, runs the rounds, and
//! exits 0 whatever the program did: a soak gates nothing. A hang-up, a termination request,
//! Ctrl+C, Ctrl+\, Ctrl+Z, or the run's bound ends a run where it waits, the lookup of the commit
//! and the build included, and what finished is still written; at the prompt, where no handler is
//! installed yet, a hang-up simply ends the command. What a build left running in its process
//! group is ended with it, and what it left in the scratch directory is removed (and a removal
//! that fails is an error). The program that runs is a private copy of the file as cargo reported
//! it, whatever replaces the file after the report. The build is given the target directory and
//! cargo's home directory as the prompt resolved them, and git no variable of its own.

#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::{
    collections::BTreeMap,
    env,
    ffi::{OsStr, OsString},
    fs, iter,
    os::unix::fs::{DirBuilderExt as _, PermissionsExt as _, symlink},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use excise_harness::{
    pty::{PtySession, SpawnSpec},
    report::{Document as _, HarnessSoak, SoakOutcome},
    safety::{kill_process, send_signal},
    scenario::Signal,
    soak::{resolve_through_ancestors, safe_path_text},
};
use serde_json::Value;

const PATIENCE: Duration = Duration::from_secs(120);

/// The commit the stand-in for `git` says the checkout is at.
const FAKE_SHA: &str = "feedfacefeedfacefeedfacefeedfacefeedface";

/// What every stand-in for cargo does first: record its arguments, the cache-cleaning setting, the
/// temporary directory, cargo's home directory, and the variables that choose a compiler wrapper
/// or a build directory, find the target directory it was given, and define what it reports.
/// `@DIR@` is the world's directory and `@MANIFEST@` the manifest of this checkout.
const CARGO_PRELUDE: &str = r#"echo "$@" >> '@DIR@/cargo-args'
echo "${CARGO_CACHE_AUTO_CLEAN_FREQUENCY-unset}" >> '@DIR@/cargo-clean'
echo "${TMPDIR-unset}" >> '@DIR@/cargo-tmp'
echo "${CARGO_HOME-unset}" >> '@DIR@/cargo-home'
for name in RUSTC_WRAPPER RUSTC_WORKSPACE_WRAPPER CARGO_BUILD_RUSTC_WRAPPER CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER CARGO_BUILD_BUILD_DIR SCCACHE_DIR; do
    eval "value=\${$name-unset}"
    echo "$name=$value" >> '@DIR@/cargo-env'
done
target=
while [ "$#" -gt 0 ]; do
    if [ "$1" = --target-dir ]; then target=$2; fi
    shift
done
install() {
    mkdir -p "$(dirname "$1")" || exit 1
    cp '@DIR@/built-excise' "$1" || exit 1
}
artifact() {
    printf '{"reason":"compiler-artifact","package_id":"excise","manifest_path":"%s","target":{"kind":["bin"],"crate_types":["bin"],"name":"excise"},"executable":"%s","fresh":false}\n' '@MANIFEST@' "$1"
}
finished() {
    printf '{"reason":"build-finished","success":true}\n'
}
"#;

/// What cargo does by default: puts a binary where the release build goes, and says so.
const BUILDS: &str = "install \"$target/release/excise\"\n\
                      artifact \"$target/release/excise\"\n\
                      finished\n";

/// A program that hangs, with a child in its process group: the id of the child is recorded.
const SLEEPS: &str = "/bin/sleep 600 &\necho $! >> '@DIR@/pids'\nwait\n";

/// A build that hangs, after it made a temporary file as a compiler does, with a child in its
/// process group: the id of the child is recorded.
const HANGS: &str = "echo partial > \"$TMPDIR/rustc-temporary\"\n\
                     /bin/sleep 600 &\n\
                     echo $! >> '@DIR@/pids'\n\
                     wait\n";

/// A program that a compiler or a build script starts and leaves running, holding the build's
/// standard output: the id of the program is recorded.
const LEAVES_A_PROGRAM: &str = "/bin/sleep 600 &\necho $! >> '@DIR@/pids'\n";

/// What the stand-in for `excise` does to show where the copy that the command made of the binary
/// is while it runs, and that it is the binary that was built.
const SHOWS_THE_COPY: &str = "for copy in '@DIR@'/scratch/xh-scratch-*/excise; do\n\
                              echo \"$copy\" >> '@DIR@/copies'\n\
                              /usr/bin/cmp -s \"$copy\" '@DIR@/built-excise' && echo same >> '@DIR@/copies'\n\
                              done\n\
                              exit 3\n";

/// A tree to soak, a script that stands in for the release build, the stand-in for `excise` that
/// it installs, a decoy for `EXCISE_E2E_BINARY`, a stand-in for `git`, and a target directory for
/// the command's output.
struct World {
    dir: tempfile::TempDir,
    target: PathBuf,
    arguments: Vec<OsString>,
    settings: Vec<(&'static str, OsString)>,
}

impl World {
    /// `built` is what the stand-in for `excise` does when it is started, after it leaves its
    /// marker. The target directory is inside the tree, or beside it.
    fn new(target_inside_the_root: bool, built: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("xt-xtask-soak-")
            .tempdir()
            .expect("a directory");
        let tree = dir.path().join("tree/Zebra-Quarterly-Ledger");
        fs::create_dir_all(&tree).expect("a tree");
        fs::write(tree.join("Payroll-7731.dat"), b"x").expect("a file");
        fs::create_dir(dir.path().join("bin")).expect("a directory for programs on the path");
        // Private to its owner whatever the umask is: the command refuses a scratch directory that
        // a group or everybody can write.
        fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.path().join("scratch"))
            .expect("a scratch directory");
        let target = if target_inside_the_root {
            dir.path().join("tree/target")
        } else {
            dir.path().join("target")
        };
        let world = Self {
            dir,
            target,
            arguments: Vec::new(),
            settings: Vec::new(),
        };
        // `@DIR@/pids` is a file in the world, which a stand-in that starts a child writes the
        // child's process id to.
        world.script(
            "built-excise",
            &format!("echo started >> '@DIR@/built-started'\n{built}"),
        );
        world.script(
            "env-excise",
            "echo started >> '@DIR@/env-started'\nexit 3\n",
        );
        world.script(
            "stale-excise",
            "echo started >> '@DIR@/stale-started'\nexit 3\n",
        );
        // The stand-in for `git` records the names of the variables of git's own that it was
        // given, and writes a line to the file that `GIT_TRACE` names, as git does.
        world.script(
            "bin/git",
            &format!(
                "echo \"$@\" >> '@DIR@/git-runs'\n\
                 pwd -P >> '@DIR@/git-cwd'\n\
                 echo \"${{GIT_DIR-unset}}\" >> '@DIR@/git-env'\n\
                 env | while IFS= read -r line; do\n\
                 case \"$line\" in GIT_*) echo \"${{line%%=*}}\" >> '@DIR@/git-vars' ;; esac\n\
                 done\n\
                 if [ -n \"${{GIT_TRACE-}}\" ]; then echo 'trace: built-in: git rev-parse HEAD' >> \"$GIT_TRACE\"; fi\n\
                 echo {FAKE_SHA}\n"
            ),
        );
        world.cargo(BUILDS);
        world
    }

    /// Writes the stand-in for cargo, which does `body` after its prelude.
    fn cargo(&self, body: &str) {
        self.script("fake-cargo", &format!("{CARGO_PRELUDE}{body}"));
    }

    fn script(&self, name: &str, body: &str) {
        let path = self.path(name);
        let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("the repository")
            .join("Cargo.toml");
        let body = body
            .replace("@DIR@", &self.dir.path().to_string_lossy())
            .replace("@MANIFEST@", &manifest.to_string_lossy());
        fs::write(&path, format!("#!/bin/sh\n{body}")).expect("a script");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("executable");
    }

    /// The same world with more arguments on the command line.
    fn with_arguments(mut self, arguments: &[&str]) -> Self {
        self.arguments
            .extend(arguments.iter().map(|argument| OsString::from(*argument)));
        self
    }

    /// The same world with an environment variable set for the command.
    fn with_env(mut self, name: &'static str, value: impl Into<OsString>) -> Self {
        self.settings.push((name, value.into()));
        self
    }

    /// The same world with the command told to use another target directory.
    fn with_target(mut self, target: PathBuf) -> Self {
        self.target = target;
        self
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// The canonical path of the tree: what the prompt asks the person to type.
    fn tree(&self) -> PathBuf {
        fs::canonicalize(self.path("tree")).expect("a tree")
    }

    /// The target directory of the command, as its environment spells it.
    fn target(&self) -> &Path {
        &self.target
    }

    /// Where `path` is, links followed and the part that does not exist yet kept: what the prompt
    /// names.
    fn canonical(path: &Path) -> String {
        resolve_through_ancestors(path)
            .to_string_lossy()
            .into_owned()
    }

    /// The complete environment of the command: the seam, the decoy, the target directory, a
    /// `PATH` with the stand-in for `git` first, and whatever the test added.
    fn env(&self) -> Vec<(OsString, OsString)> {
        let path = env::join_paths(
            iter::once(self.path("bin"))
                .chain(env::split_paths(&env::var_os("PATH").unwrap_or_default())),
        )
        .expect("a PATH");
        let mut vars: BTreeMap<OsString, OsString> = BTreeMap::new();
        vars.insert("CARGO".into(), self.path("fake-cargo").into_os_string());
        vars.insert(
            "EXCISE_E2E_BINARY".into(),
            self.path("env-excise").into_os_string(),
        );
        vars.insert(
            "CARGO_TARGET_DIR".into(),
            self.target.clone().into_os_string(),
        );
        vars.insert("PATH".into(), path);
        vars.insert("HOME".into(), self.dir.path().as_os_str().to_owned());
        vars.insert("TERM".into(), "xterm-256color".into());
        vars.insert(
            "EXCISE_E2E_TMPDIR".into(),
            self.path("scratch").into_os_string(),
        );
        for (name, value) in &self.settings {
            vars.insert((*name).into(), value.clone());
        }
        vars.into_iter().collect()
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .arg("soak")
            .arg(self.tree())
            .args(&self.arguments)
            .envs(self.env());
        command
    }

    fn lines(&self, name: &str) -> Vec<String> {
        fs::read_to_string(self.path(name))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// The arguments the build was run with, one entry per run.
    fn builds(&self) -> Vec<String> {
        self.lines("cargo-args")
    }

    /// What `CARGO_CACHE_AUTO_CLEAN_FREQUENCY` was for each run of the build.
    fn cache_cleaning(&self) -> Vec<String> {
        self.lines("cargo-clean")
    }

    /// What `TMPDIR` was for each run of the build.
    fn build_tmpdirs(&self) -> Vec<String> {
        self.lines("cargo-tmp")
    }

    /// What the build saw of the variables that choose a compiler wrapper or a build directory
    /// (and of a wrapper's own setting), one `NAME=value` entry for each, per run of the build:
    /// `unset` for a variable that was not set, and nothing after the `=` for one set to nothing.
    fn cargo_env(&self) -> Vec<String> {
        self.lines("cargo-env")
    }

    /// The directory `git` ran in, one entry per run.
    fn git_cwds(&self) -> Vec<String> {
        self.lines("git-cwd")
    }

    /// What `GIT_DIR` was for each run of `git`.
    fn git_envs(&self) -> Vec<String> {
        self.lines("git-env")
    }

    /// The names of the variables of git's own that `git` was given, one entry per variable and
    /// run.
    fn git_vars(&self) -> Vec<String> {
        self.lines("git-vars")
    }

    /// What `CARGO_HOME` was for each run of the build.
    fn build_cargo_homes(&self) -> Vec<String> {
        self.lines("cargo-home")
    }

    /// What is left in the scratch directory, by name.
    fn scratch_entries(&self) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(self.path("scratch"))
            .expect("the scratch directory")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    /// The arguments `git` was run with, one entry per run.
    fn git_runs(&self) -> Vec<String> {
        self.lines("git-runs")
    }

    /// The process ids that stand-ins recorded for the children they started.
    fn recorded_pids(&self) -> Vec<u32> {
        self.lines("pids")
            .iter()
            .filter_map(|line| line.trim().parse().ok())
            .collect()
    }

    fn built_excise_started(&self) -> bool {
        self.path("built-started").exists()
    }

    fn decoy_started(&self) -> bool {
        self.path("env-started").exists()
    }

    fn stale_started(&self) -> bool {
        self.path("stale-started").exists()
    }

    /// Whether the command started the build, `git`, or any stand-in for `excise`.
    fn anything_started(&self) -> bool {
        !self.builds().is_empty()
            || !self.git_runs().is_empty()
            || self.built_excise_started()
            || self.decoy_started()
            || self.stale_started()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // A stand-in that hangs starts a child, `sleep 600`, and records its process id. A test
        // that fails before it has seen the child end would leave it running for ten minutes, and
        // the temporary directory removes only files. So the world ends what it recorded, unless
        // the number has since been given to another process.
        for pid in self.recorded_pids() {
            if command_line(pid).is_some_and(|line| line == "/bin/sleep 600") {
                let _ = kill_process(pid);
            }
        }
    }
}

fn flat(session: &PtySession) -> String {
    session
        .screen()
        .text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn wait(session: &mut PtySession, mut done: impl FnMut(&PtySession) -> bool) {
    let deadline = Instant::now() + PATIENCE;
    loop {
        session.pump().expect("the terminal is read");
        if done(session) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "gave up waiting; the screen shows: {}",
            flat(session)
        );
        thread::sleep(Duration::from_millis(10));
    }
}

/// The checkout this crate is in: where `cargo xtask` runs the command.
fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the repository")
        .to_path_buf()
}

/// Starts the command in a pseudo-terminal, in the checkout, with an asciicast of what it writes
/// when `recording` says where.
fn start(world: &World, recording: Option<PathBuf>) -> PtySession {
    start_in(world, repository(), recording)
}

/// Like `start`, in the directory `cwd`.
fn start_in(world: &World, cwd: PathBuf, recording: Option<PathBuf>) -> PtySession {
    let command = world.command();
    PtySession::spawn(&SpawnSpec {
        program: PathBuf::from(command.get_program()),
        args: command.get_args().map(Into::into).collect(),
        env: world.env(),
        cwd,
        cols: 200,
        rows: 100,
        drain_bytes_per_sec: None,
        recording,
        title: None,
    })
    .expect("the command starts in a pseudo-terminal")
}

/// Starts the command in a pseudo-terminal and waits for its prompt.
fn at_the_prompt(world: &World) -> PtySession {
    at_the_prompt_in(world, repository())
}

/// Starts the command in a pseudo-terminal in the directory `cwd` and waits for its prompt.
fn at_the_prompt_in(world: &World, cwd: PathBuf) -> PtySession {
    let mut session = start_in(world, cwd, None);
    wait(&mut session, |session| {
        flat(session).contains("or anything else to stop:")
    });
    session
}

/// Types `typed` and Enter, waits for the command to end, and returns what the person saw and how
/// it ended.
fn type_and_finish(session: &mut PtySession, typed: &str) -> (String, Option<i64>) {
    session
        .send(format!("{typed}\r").as_bytes())
        .expect("the line is typed");
    finish(session)
}

fn finish(session: &mut PtySession) -> (String, Option<i64>) {
    wait(session, |session| session.exit().is_some());
    session
        .drain(Duration::from_millis(50), Duration::from_millis(500))
        .expect("what is left is read");
    let code = session.exit().and_then(|exit| exit.code).map(i64::from);
    (flat(session), code)
}

/// Runs the command to its prompt, answers it with something that is not the root, and returns
/// the prompt as the person read it. The environment of the command is what `settings` says on top
/// of the world's.
fn prompt_with(settings: impl FnOnce(&World) -> Vec<(&'static str, PathBuf)>) -> (World, String) {
    let mut world = World::new(false, "exit 3\n");
    for (name, value) in settings(&world) {
        world = world.with_env(name, value);
    }
    let mut session = at_the_prompt(&world);
    let prompt = flat(&session);
    let (screen, code) = type_and_finish(&mut session, "no");
    assert_eq!(code, Some(1), "{screen}");
    assert!(!world.anything_started(), "something was started: {screen}");
    (world, prompt)
}

/// What the command wrote to the terminal, from an asciicast: the text it wrote, as it wrote it,
/// before any screen model saw it. The recording is complete once the session has finished it.
fn transcript(cast: &Path) -> String {
    fs::read_to_string(cast)
        .expect("the recording")
        .lines()
        .skip(1)
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event[1].as_str() == Some("o"))
        .filter_map(|event| event[2].as_str().map(str::to_owned))
        .collect()
}

/// Every entry below `root` by relative path: a directory as an empty entry, a file as its bytes.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(dir: &Path, relative: &str, into: &mut BTreeMap<String, Vec<u8>>) {
        for entry in fs::read_dir(dir).expect("a directory") {
            let entry = entry.expect("an entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let key = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let kind = entry.file_type().expect("a file type");
            if kind.is_dir() {
                into.insert(format!("{key}/"), Vec::new());
                walk(&entry.path(), &key, into);
            } else if kind.is_symlink() {
                let target = fs::read_link(entry.path()).expect("a link");
                into.insert(key, target.to_string_lossy().into_owned().into_bytes());
            } else {
                into.insert(key, fs::read(entry.path()).expect("a file"));
            }
        }
    }
    let mut into = BTreeMap::new();
    walk(root, "", &mut into);
    into
}

/// The document of the one run that the command made in the world's output directory.
fn only_run(world: &World) -> HarnessSoak {
    let runs: Vec<PathBuf> = fs::read_dir(world.target().join("excise-soak"))
        .expect("the output directory")
        .map(|entry| entry.expect("an entry"))
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    let [run] = runs.as_slice() else {
        panic!("one run: {runs:?}");
    };
    let summary = fs::read_to_string(run.join("summary.json")).expect("summary.json");
    HarnessSoak::from_json_str(&summary).expect("a document that validates")
}

/// The command line of the process, when there is such a process.
fn command_line(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Whether the process is there and is not a zombie.
fn alive(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stderr(Stdio::null())
        .output()
        .expect("ps runs");
    let state = String::from_utf8_lossy(&output.stdout);
    output.status.success() && !state.trim().is_empty() && !state.trim().starts_with('Z')
}

/// Waits for the processes to be gone, which the command ends with the process group they are in.
fn assert_gone(pids: &[u32]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while pids.iter().any(|pid| alive(*pid)) {
        assert!(
            Instant::now() < deadline,
            "a child of the program is still running: {pids:?}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn cargo_xtask_is_an_alias_of_cargo_run_so_cargo_builds_and_starts_xtask_before_it_can_ask() {
    // The documents and the prompt say that cargo has built and started xtask before xtask can
    // read a terminal or the prompt, as it does for every `cargo xtask` command, and that the soak
    // cannot change it. They say it because of this alias, which no test of the soak can alter.
    let config = fs::read_to_string(repository().join(".cargo/config.toml"))
        .expect("the cargo configuration of the checkout");

    assert!(
        config
            .lines()
            .any(|line| line.trim() == r#"xtask = "run --locked --package xtask --""#),
        "`cargo xtask` is no longer an alias of `cargo run --package xtask --`, which the \
         documents of the soak (the harness README, docs/development.md, the prompt) say it is:\n\
         {config}"
    );
}

#[test]
fn the_xtask_binary_run_without_a_terminal_refuses_before_it_builds_or_starts_anything() {
    let world = World::new(false, "exit 3\n");

    let output = world
        .command()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("the command runs");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is for a person at a terminal"), "{stderr}");
    assert!(stderr.contains("an agent must not run it"), "{stderr}");
    assert!(
        output.stdout.is_empty(),
        "no prompt is shown to something that is not a person"
    );
    assert!(
        !world.anything_started(),
        "the build, git, or a program was run"
    );
    assert!(!world.target().exists(), "something was built or written");
}

#[test]
fn the_xtask_binary_refuses_a_mistyped_path_at_its_prompt_before_it_builds_or_starts_anything() {
    let world = World::new(false, "exit 3\n");
    let tree = world.tree().to_string_lossy().into_owned();

    for typed in [
        "/not/the/root".to_owned(),
        String::new(),
        "y".to_owned(),
        format!("{tree}/"),
        format!("{tree}x"),
    ] {
        let mut session = at_the_prompt(&world);
        let (screen, code) = type_and_finish(&mut session, &typed);

        assert_eq!(code, Some(1), "{typed:?}: {screen}");
        assert!(
            screen.contains("that is not the root's full path, so nothing was started"),
            "{typed:?}: {screen}"
        );
        assert!(
            !world.anything_started(),
            "{typed:?}: the build, git, or a program was run"
        );
        assert!(!world.target().exists(), "{typed:?}: something was written");
    }
}

#[test]
fn a_scratch_directory_that_others_can_change_is_refused_before_the_prompt_and_a_sticky_one_is_not()
{
    let world = World::new(false, "exit 3\n");
    // Everybody can write it, and it is not sticky: another user could rename what the soak puts
    // in it, among them the copy of the program that it runs on the tree.
    fs::set_permissions(world.path("scratch"), fs::Permissions::from_mode(0o777))
        .expect("opened up");

    let mut session = start(&world, None);
    wait(&mut session, |session| {
        session.exit().is_some() || flat(session).contains("or anything else to stop:")
    });
    assert!(
        session.exit().is_some(),
        "it asked for the root's path although others can change the scratch directory: {}",
        flat(&session)
    );
    let (screen, code) = finish(&mut session);

    assert_eq!(code, Some(1), "{screen}");
    assert!(
        screen.contains("the scratch directory is not private to you")
            && screen.contains("can be written by everybody and is not sticky")
            && screen.contains("Nothing was built or started"),
        "{screen}"
    );
    assert!(
        !screen.contains("Type the directory's full path"),
        "the prompt was shown: {screen}"
    );
    assert!(!world.anything_started(), "{screen}");
    assert!(!world.target().exists(), "something was written");

    // The sticky bit stops another user from renaming what is in it: the command goes on.
    fs::set_permissions(world.path("scratch"), fs::Permissions::from_mode(0o1777)).expect("sticky");
    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, "no");
    assert_eq!(code, Some(1), "{screen}");
    assert!(
        screen.contains("that is not the root's full path, so nothing was started"),
        "{screen}"
    );
    assert!(!world.anything_started(), "{screen}");
}

#[test]
fn a_target_directory_that_others_can_change_gets_a_note_before_the_prompt_and_no_refusal() {
    let world = World::new(false, "exit 3\n");
    let open = world.path("open");
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&open)
        .expect("a directory");
    fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).expect("opened up");
    let world = world.with_target(open.join("target"));

    let mut session = at_the_prompt(&world);
    let shown = flat(&session);
    let (screen, code) = type_and_finish(&mut session, "no");

    assert!(
        shown.contains("note: the directory the build is made in is not private to you")
            && shown.contains("can be written by everybody and is not sticky")
            && shown.contains("The soak goes on"),
        "{shown}"
    );
    assert_eq!(code, Some(1), "{screen}");
    assert!(!world.anything_started(), "{screen}");
}

#[test]
fn a_hang_up_at_the_prompt_ends_the_command_at_once_with_nothing_started() {
    // No handler is installed before the confirmation: one would make Ctrl+C do nothing while the
    // line is read, so the signal ends the command as it ends any program.
    let world = World::new(false, "exit 3\n");
    let mut session = at_the_prompt(&world);

    send_signal(session.pid(), Signal::Hup).expect("the hang-up is delivered");
    let (screen, code) = finish(&mut session);

    assert_eq!(code, None, "it was ended by the signal: {screen}");
    assert!(!world.anything_started(), "{screen}");
    assert!(!world.target().exists(), "something was written");
}

#[test]
fn the_right_path_builds_and_runs_the_release_build_and_never_the_binary_of_the_environment() {
    let world = World::new(false, "exit 3\n");
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let prompt = flat(&session);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert!(
        prompt.contains(&tree) && prompt.contains("never Backspace"),
        "the prompt says what the soak is of and why it cannot delete: {prompt}"
    );
    assert!(
        prompt.contains("release build of this checkout (HEAD at the time you confirm,"),
        "the prompt says what will run, and not which commit: {prompt}"
    );
    assert!(
        !prompt.contains(&FAKE_SHA[..12]),
        "the commit is looked up after the confirmation, so the prompt cannot name it: {prompt}"
    );
    assert!(
        prompt.contains("write nothing in that directory"),
        "{prompt}"
    );
    assert_eq!(
        world.builds(),
        [format!(
            "build --release --locked --offline -p excise --target-dir {} \
             --message-format json-render-diagnostics",
            World::canonical(world.target())
        )],
        "the soak builds the release binary of this checkout, offline, in the directory that the \
         prompt named, where it resolves"
    );
    assert_eq!(
        world.cache_cleaning(),
        ["never"],
        "cargo's cache cleaning is off for the build"
    );
    assert_eq!(
        world.build_cargo_homes(),
        [World::canonical(&world.path(".cargo"))],
        "with no CARGO_HOME set, the build is given `.cargo` in the home directory, as the prompt \
         resolved it"
    );
    let scratch = fs::canonicalize(world.path("scratch")).expect("the scratch directory");
    let tmpdirs = world.build_tmpdirs();
    let [tmpdir] = tmpdirs.as_slice() else {
        panic!("the build ran once: {tmpdirs:?}");
    };
    let tmpdir = Path::new(tmpdir);
    assert_eq!(
        tmpdir.file_name(),
        Some(OsStr::new("tmp")),
        "the build's temporary files are in the `tmp` directory of an area of its own"
    );
    let area = tmpdir.parent().expect("the area of the build");
    assert_eq!(
        area.parent(),
        Some(scratch.as_path()),
        "the area is made in the scratch directory, outside the root, as one absolute path"
    );
    assert!(
        area.file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("xh-scratch-")),
        "a name that a check for the harness's residue would catch: {area:?}"
    );
    assert_eq!(
        world.git_runs(),
        ["rev-parse HEAD"],
        "the commit was looked up once, after the confirmation"
    );
    assert!(
        screen.contains(&format!("HEAD {}", &FAKE_SHA[..12])),
        "the commit that is built is said: {screen}"
    );
    assert!(
        world.built_excise_started(),
        "the soak ran the build it made"
    );
    assert!(
        !world.decoy_started(),
        "EXCISE_E2E_BINARY replaced the release build"
    );
    assert!(
        screen.contains("summary.json") && screen.contains("quirks.txt"),
        "{screen}"
    );
    assert_eq!(
        world.scratch_entries(),
        Vec::<String>::new(),
        "the area of the build, with its temporary files and the copy of the binary, was removed"
    );

    let document = only_run(&world);
    assert_eq!(document.outcome, SoakOutcome::Finished);
    assert_eq!(document.git_sha, FAKE_SHA, "the commit git printed");
    assert_eq!(document.headless.len(), 1);
    assert_eq!(document.tui.len(), 2);
    assert!(
        !document.quirks.is_empty(),
        "the program failed, which is a quirk and not a failure of the soak"
    );
}

#[test]
fn a_target_directory_inside_the_root_is_named_before_the_path_is_asked_and_is_the_only_place_written()
 {
    let world = World::new(true, "exit 3\n");
    let tree = world.tree().to_string_lossy().into_owned();
    let before = snapshot(&world.tree());

    let mut session = at_the_prompt(&world);
    let prompt = flat(&session);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert!(
        prompt.contains("in one place only")
            && prompt.contains(&World::canonical(world.target()))
            && prompt.contains("`latest`"),
        "the one place written inside the root is named before the path is asked: {prompt}"
    );
    assert!(
        !prompt.contains(".global-cache"),
        "cargo's home is outside the root: {prompt}"
    );
    assert_eq!(code, Some(0), "{screen}");
    let after = snapshot(&world.tree());
    let outside_the_target: Vec<&String> = after
        .keys()
        .filter(|key| !key.starts_with("target/") && *key != "target/")
        .collect();
    let before_outside: Vec<&String> = before.keys().collect();
    assert_eq!(
        outside_the_target, before_outside,
        "nothing outside the target directory was added or removed"
    );
    for (key, bytes) in &before {
        assert_eq!(after.get(key), Some(bytes), "{key} changed");
    }
    assert!(
        after.contains_key("target/release/excise"),
        "the build was written there"
    );
    assert!(
        after.keys().any(|key| key.ends_with("/summary.json")),
        "and so was the soak's own output"
    );
}

#[test]
fn a_target_directory_that_is_a_link_into_the_root_is_named_where_it_resolves() {
    let world = World::new(false, "exit 3\n");
    let real = world.tree().join("real-target");
    fs::create_dir(&real).expect("a directory in the tree");
    let link = world.path("link-to-the-target");
    symlink(&real, &link).expect("a link");
    let world = world.with_target(link);

    let mut session = at_the_prompt(&world);
    let prompt = flat(&session);
    let (screen, code) = type_and_finish(&mut session, "no");

    assert_eq!(code, Some(1), "{screen}");
    assert!(
        prompt.contains("in one place only") && prompt.contains(&*real.to_string_lossy()),
        "the link leads into the root, so what it leads to is the place written: {prompt}"
    );
    assert!(
        !prompt.contains("write nothing in that directory"),
        "{prompt}"
    );
    assert!(!world.anything_started(), "{screen}");
}

#[test]
fn a_symbolic_link_as_the_output_directory_is_refused_before_the_prompt_and_writes_nothing() {
    for into_the_root in [true, false] {
        let world = World::new(false, "exit 3\n");
        let destination = if into_the_root {
            world.tree().join("somewhere")
        } else {
            world.path("elsewhere")
        };
        fs::create_dir(&destination).expect("a directory");
        fs::create_dir(world.target()).expect("a target directory");
        let link = world.target().join("excise-soak");
        symlink(&destination, &link).expect("a link");
        let tree_before = snapshot(&world.tree());
        let destination_before = snapshot(&destination);

        let mut session = start(&world, None);
        let (screen, code) = finish(&mut session);

        assert_eq!(code, Some(1), "{into_the_root}: {screen}");
        assert!(
            screen.contains("is a symbolic link") && screen.contains("cannot take a run"),
            "{into_the_root}: {screen}"
        );
        assert!(
            screen.contains(&safe_path_text(&link)),
            "{into_the_root}: it names the link: {screen}"
        );
        assert!(
            !screen.contains("or anything else to stop:"),
            "{into_the_root}: no prompt is shown: {screen}"
        );
        assert!(
            !world.anything_started(),
            "{into_the_root}: the build, git, or a program was run"
        );
        assert_eq!(
            snapshot(&world.tree()),
            tree_before,
            "{into_the_root}: the root is byte for byte as it was"
        );
        assert_eq!(
            snapshot(&destination),
            destination_before,
            "{into_the_root}: nothing was written through the link"
        );
    }
}

#[test]
fn a_root_inside_a_directory_that_the_build_writes_in_is_refused_before_the_prompt() {
    // The directory that holds the tree is the target directory, and then cargo's home.
    for (variable, what) in [
        ("CARGO_TARGET_DIR", "the directory the build is made in"),
        ("CARGO_HOME", "cargo's home directory"),
    ] {
        let world = World::new(false, "exit 3\n");
        let holder = world.dir.path().to_path_buf();
        let world = if variable == "CARGO_TARGET_DIR" {
            world.with_target(holder)
        } else {
            world.with_env("CARGO_HOME", holder)
        };
        let tree_before = snapshot(&world.tree());

        let mut session = start(&world, None);
        let (screen, code) = finish(&mut session);

        assert_eq!(code, Some(1), "{variable}: {screen}");
        assert!(
            screen.contains(&format!("the root lies inside {what}")),
            "{variable}: {screen}"
        );
        assert!(
            screen.contains(&format!("move it with {variable}")),
            "{variable}: it says how to move the place: {screen}"
        );
        assert!(
            !screen.contains("or anything else to stop:"),
            "{variable}: no prompt is shown: {screen}"
        );
        assert!(
            !world.anything_started(),
            "{variable}: the build, git, or a program was run"
        );
        assert_eq!(
            snapshot(&world.tree()),
            tree_before,
            "{variable}: the root is byte for byte as it was"
        );
    }
}

#[test]
fn a_root_that_holds_the_scratch_directory_is_refused_before_the_prompt() {
    let world = World::new(false, "exit 3\n");
    let inside = world.tree().join("scratch");
    fs::create_dir(&inside).expect("a scratch directory in the tree");
    let world = world.with_env("EXCISE_E2E_TMPDIR", inside.clone());
    let tree_before = snapshot(&world.tree());

    let mut session = start(&world, None);
    let (screen, code) = finish(&mut session);

    assert_eq!(code, Some(1), "{screen}");
    assert!(
        screen.contains("the scratch directory") && screen.contains("is inside the root"),
        "{screen}"
    );
    assert!(
        screen.contains("EXCISE_E2E_TMPDIR"),
        "it says how to move the scratch directory: {screen}"
    );
    assert!(
        !screen.contains("or anything else to stop:"),
        "no prompt is shown: {screen}"
    );
    assert!(
        !world.anything_started(),
        "the build, git, or a program was run"
    );
    assert_eq!(
        snapshot(&world.tree()),
        tree_before,
        "the root is byte for byte as it was"
    );
}

#[test]
fn cargos_home_is_named_at_the_prompt_when_it_lies_inside_the_root_and_not_when_it_does_not() {
    // CARGO_HOME inside the root, and outside it; and, with none set, the home directory inside it,
    // where `.cargo` is cargo's home, and with CARGO_HOME set to nothing, which cargo ignores.
    let (by_variable_world, by_variable) =
        prompt_with(|world| vec![("CARGO_HOME", world.tree().join("cargo-home"))]);
    let (_, outside) = prompt_with(|world| vec![("CARGO_HOME", world.path("cargo-home"))]);
    let (by_home_world, by_home) = prompt_with(|world| vec![("HOME", world.tree().join("home"))]);
    let (empty_world, empty) = prompt_with(|world| {
        vec![
            ("CARGO_HOME", PathBuf::new()),
            ("HOME", world.tree().join("home")),
        ]
    });

    for (home, prompt, why) in [
        (
            by_variable_world.tree().join("cargo-home"),
            &by_variable,
            "by CARGO_HOME",
        ),
        (
            by_home_world.tree().join("home/.cargo"),
            &by_home,
            "by HOME",
        ),
        (
            empty_world.tree().join("home/.cargo"),
            &empty,
            "by HOME, with CARGO_HOME empty",
        ),
    ] {
        assert!(
            prompt.contains("in one place only")
                && prompt.contains(&World::canonical(&home))
                && prompt.contains(".global-cache")
                && prompt.contains(".package-cache")
                && prompt.contains("downloads nothing and cleans nothing"),
            "cargo's home is inside the root ({why}), so it is named with what is written there: \
             {prompt}"
        );
        assert!(
            !prompt.contains("`latest`") && !prompt.contains("write nothing"),
            "the soak's own files are outside the root ({why}): {prompt}"
        );
    }
    assert!(
        outside.contains("write nothing in that directory")
            && !outside.contains(".global-cache")
            && !outside.contains("cargo-home"),
        "a cargo home outside the root is not named: {outside}"
    );
}

#[test]
fn a_target_directory_with_control_characters_is_shown_escaped_and_never_raw() {
    // A line break, a screen clear, a window title, a bell, and a bidirectional override.
    let world = World::new(false, "exit 3\n");
    let hostile = world.path("evil\n\u{1b}[2J\u{1b}]0;pwned\u{7}\u{202e}end");
    fs::create_dir(&hostile).expect("a directory whose name holds control characters");
    let world = world.with_target(hostile.clone());
    let cast = world.path("session.cast");

    let mut session = start(&world, Some(cast.clone()));
    wait(&mut session, |session| {
        flat(session).contains("or anything else to stop:")
    });
    let prompt = flat(&session);
    let (screen, code) = type_and_finish(&mut session, "no");
    session
        .finish_recording()
        .expect("the recording is complete");

    assert_eq!(code, Some(1), "{screen}");
    let escaped = safe_path_text(&resolve_through_ancestors(&hostile));
    assert!(
        prompt.contains(&escaped),
        "the target directory is shown escaped, as `{escaped}`: {prompt}"
    );
    assert!(
        prompt.contains(r"evil\n\x1b[2J\x1b]0;pwned") && prompt.contains(r"\u{202e}end"),
        "{prompt}"
    );
    let written = transcript(&cast);
    for planted in ["\u{1b}[2J", "\u{1b}]0;pwned", "\u{202e}", "\u{7}"] {
        assert!(
            !written.contains(planted),
            "{planted:?} reached the terminal as it is: {written:?}"
        );
    }
    assert!(
        written.contains(r"evil\n\x1b[2J\x1b]0;pwned"),
        "what the terminal received is the escaped text: {written:?}"
    );
    assert!(!world.anything_started(), "{screen}");
    assert!(
        !hostile.join("excise-soak").exists(),
        "nothing is written in the target directory before the confirmation"
    );
}

#[test]
fn the_binary_that_runs_is_the_one_cargo_reported_and_not_the_one_at_the_unqualified_path() {
    // `build.target` puts the binary in `<target>/<triple>/release/`, and a stale build can be
    // where a path that is not told better would look.
    let world = World::new(false, "exit 3\n");
    world.cargo(
        "install \"$target/x86_64-unknown-test/release/excise\"\n\
         mkdir -p \"$target/release\"\n\
         cp '@DIR@/stale-excise' \"$target/release/excise\"\n\
         artifact \"$target/x86_64-unknown-test/release/excise\"\n\
         finished\n",
    );
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert!(
        world.built_excise_started(),
        "the binary cargo reported was run: {screen}"
    );
    assert!(
        !world.stale_started(),
        "the binary at the unqualified path was run: {screen}"
    );
    assert!(!world.decoy_started());
    assert_eq!(only_run(&world).outcome, SoakOutcome::Finished);
}

#[test]
fn a_build_that_does_not_give_one_usable_binary_starts_nothing_and_writes_nothing() {
    let cases = [
        (
            "reports no binary",
            "install \"$target/release/excise\"\nfinished\n",
            "reported no executable for the `excise` binary of this checkout",
        ),
        (
            "reports two",
            "install \"$target/release/excise\"\n\
             artifact \"$target/release/excise\"\n\
             artifact \"$target/release/excise\"\n\
             finished\n",
            "reported 2 executables for the `excise` binary of this checkout",
        ),
        (
            "reports a file that is not there",
            "artifact \"$target/release/excise\"\nfinished\n",
            "which cannot be inspected",
        ),
        (
            "reports a binary outside the target directory",
            "mkdir -p \"$target\"\n\
             install '@DIR@/elsewhere/excise'\n\
             artifact '@DIR@/elsewhere/excise'\n\
             finished\n",
            "which is outside the target directory that the build was told to use",
        ),
        (
            "reports a FIFO",
            "mkdir -p \"$target/release\"\n\
             mkfifo \"$target/release/excise\"\n\
             artifact \"$target/release/excise\"\n\
             finished\n",
            "which is not a regular file",
        ),
        (
            "reports a link",
            "install \"$target/release/real-excise\"\n\
             ln -s real-excise \"$target/release/excise\"\n\
             artifact \"$target/release/excise\"\n\
             finished\n",
            "which is not a regular file",
        ),
    ];
    for (why, body, said) in cases {
        let world = World::new(false, "exit 3\n");
        world.cargo(body);
        let tree = world.tree().to_string_lossy().into_owned();

        let mut session = at_the_prompt(&world);
        let (screen, code) = type_and_finish(&mut session, &tree);

        assert_eq!(code, Some(1), "a build that {why}: {screen}");
        assert!(
            screen.contains(said),
            "a build that {why} says so: {screen}"
        );
        assert_eq!(world.builds().len(), 1, "a build that {why} ran once");
        assert!(
            !world.built_excise_started() && !world.decoy_started() && !world.stale_started(),
            "a build that {why}: a program was started: {screen}"
        );
        assert!(
            !world.target().join("excise-soak").exists(),
            "a build that {why}: something was written in the output directory"
        );
    }
}

#[test]
fn a_build_that_fails_says_it_is_offline_and_how_the_crates_are_fetched() {
    let world = World::new(false, "exit 3\n");
    world.cargo("echo 'error: no matching package named `serde` found' >&2\nexit 101\n");
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(1), "{screen}");
    assert!(
        screen.contains("building the release binary failed (exit status: 101)"),
        "{screen}"
    );
    assert!(
        screen.contains("The build is offline and downloads nothing")
            && screen.contains("run `cargo fetch --locked` yourself"),
        "the person is told what to do about a crate that is missing: {screen}"
    );
    assert!(
        screen.contains("no matching package named `serde` found"),
        "cargo's own output reaches the person: {screen}"
    );
    assert!(!world.built_excise_started() && !world.decoy_started());
    assert!(!world.target().join("excise-soak").exists());
}

#[test]
fn a_hang_up_ends_the_run_where_it_waits_and_what_finished_is_still_written() {
    // The program starts a child in its own process group and hangs.
    let world = World::new(false, "/bin/sleep 600 &\necho $! >> '@DIR@/pids'\nwait\n");
    let tree = world.tree().to_string_lossy().into_owned();
    let mut session = at_the_prompt(&world);
    session
        .send(format!("{tree}\r").as_bytes())
        .expect("the line is typed");
    wait(&mut session, |_| world.built_excise_started());

    send_signal(session.pid(), Signal::Hup).expect("the hang-up is delivered");
    let (screen, code) = finish(&mut session);

    assert_eq!(code, Some(1), "{screen}");
    assert!(screen.contains("interrupted"), "{screen}");
    let document = only_run(&world);
    assert_eq!(document.outcome, SoakOutcome::Interrupted);
    assert_eq!(document.headless.len(), 1, "what finished is recorded");
    // The program's process group was killed with it: the child it started is gone.
    let pids = world.recorded_pids();
    assert_eq!(pids.len(), 1, "{pids:?}");
    assert_gone(&pids);
}

#[test]
fn a_signal_that_asks_the_soak_to_stop_ends_a_build_with_its_process_group_and_starts_nothing() {
    // A hang-up, a termination request, Ctrl+C, and Ctrl+\.
    for signal in [Signal::Hup, Signal::Term, Signal::Int, Signal::Quit] {
        let world = World::new(false, "exit 3\n");
        world.cargo(HANGS);
        let tree = world.tree().to_string_lossy().into_owned();
        let mut session = at_the_prompt(&world);
        session
            .send(format!("{tree}\r").as_bytes())
            .expect("the line is typed");
        wait(&mut session, |_| !world.recorded_pids().is_empty());

        send_signal(session.pid(), signal).expect("the signal is delivered");
        let (screen, code) = finish(&mut session);

        assert_eq!(code, Some(1), "{signal}: {screen}");
        assert!(
            screen.contains("the build was interrupted, so nothing was run"),
            "{signal}: {screen}"
        );
        assert!(
            !world.built_excise_started() && !world.decoy_started(),
            "{signal}: a program was started"
        );
        assert!(
            !world.target().join("excise-soak").exists(),
            "{signal}: something was written in the output directory"
        );
        let pids = world.recorded_pids();
        assert_eq!(pids.len(), 1, "{signal}: {pids:?}");
        assert_gone(&pids);
        assert_eq!(
            world.scratch_entries(),
            Vec::<String>::new(),
            "{signal}: what the killed build left in its temporary directory was removed"
        );
    }
}

#[test]
fn ctrl_z_asks_the_soak_to_stop_and_does_not_stop_the_command() {
    // Ctrl+Z is SIGTSTP. A command that it stopped would stop its clock and its watch on the
    // report with it, while the build and the scans, in process groups of their own that a stop of
    // the terminal does not reach, went on. It stops the run where it waits, as a hang-up does.
    let world = World::new(false, "exit 3\n");
    world.cargo(HANGS);
    let tree = world.tree().to_string_lossy().into_owned();
    let mut session = at_the_prompt(&world);
    session
        .send(format!("{tree}\r").as_bytes())
        .expect("the line is typed");
    wait(&mut session, |_| !world.recorded_pids().is_empty());

    let sent = std::process::Command::new("kill")
        .args(["-TSTP", &session.pid().to_string()])
        .status()
        .expect("kill runs");
    assert!(sent.success());
    let (screen, code) = finish(&mut session);

    assert_eq!(code, Some(1), "{screen}");
    assert!(
        screen.contains("the build was interrupted, so nothing was run"),
        "{screen}"
    );
    let pids = world.recorded_pids();
    assert_eq!(pids.len(), 1, "{pids:?}");
    assert_gone(&pids);
    assert!(
        !world.target().join("excise-soak").exists(),
        "nothing is written in the output directory"
    );
}

#[test]
fn the_bound_on_the_run_ends_a_build_that_does_not_end_the_same_way() {
    // Long enough that a loaded machine has recorded the child's process id before the bound
    // passes, short enough to be a test.
    let world = World::new(false, "exit 3\n").with_arguments(&["--timeout", "5s"]);
    world.cargo(HANGS);
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(1), "{screen}");
    assert!(
        screen.contains("the build passed the bound on the run, so nothing was run"),
        "{screen}"
    );
    assert!(!world.built_excise_started() && !world.decoy_started());
    assert!(
        !world.target().join("excise-soak").exists(),
        "nothing is written in the output directory"
    );
    let pids = world.recorded_pids();
    assert_eq!(pids.len(), 1, "{pids:?}");
    assert_gone(&pids);
    assert_eq!(
        world.scratch_entries(),
        Vec::<String>::new(),
        "what the killed build left in its temporary directory was removed"
    );
}

#[test]
fn the_document_records_the_bound_as_the_person_set_it_whatever_the_build_used_of_it() {
    let world = World::new(false, "exit 3\n").with_arguments(&["--timeout", "20s"]);
    world.cargo(&format!("sleep 3\n{BUILDS}"));
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert_eq!(
        only_run(&world).limits.run_ms,
        20_000,
        "the bound of the whole run is what the person set, and the three seconds that the build \
         took are not taken off it"
    );
}

#[test]
fn the_build_counts_against_the_bound_and_the_scans_get_only_the_time_that_is_left_of_it() {
    // The bound is eight seconds, counted from the confirmation, and the build takes four of them.
    // The program hangs, so a scan ends only when the run's deadline comes: if the clock were
    // counted again from when the soak starts, the scan would run for the whole eight.
    let world = World::new(false, SLEEPS).with_arguments(&["--timeout", "8s"]);
    world.cargo(&format!("sleep 4\n{BUILDS}"));
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(1), "{screen}");
    assert!(screen.contains("interrupted"), "{screen}");
    let document = only_run(&world);
    assert_eq!(document.outcome, SoakOutcome::Interrupted);
    let [scan] = document.headless.as_slice() else {
        panic!("one scan was started: {:?}", document.headless);
    };
    assert!(
        scan.wall_ms < 6_000.0,
        "the build used four of the eight seconds, so a scan has four at most: {} ms",
        scan.wall_ms
    );
    let pids = world.recorded_pids();
    assert_eq!(pids.len(), 1, "{pids:?}");
    assert_gone(&pids);
}

#[test]
fn what_a_build_leaves_running_is_ended_with_the_build_whether_it_succeeded_or_failed() {
    for (why, body, expected) in [
        (
            "a build that succeeds",
            format!("{LEAVES_A_PROGRAM}{BUILDS}"),
            0,
        ),
        (
            "a build that fails",
            format!("{LEAVES_A_PROGRAM}exit 101\n"),
            1,
        ),
    ] {
        let world = World::new(false, "exit 3\n");
        world.cargo(&body);
        let tree = world.tree().to_string_lossy().into_owned();

        let mut session = at_the_prompt(&world);
        let (screen, code) = type_and_finish(&mut session, &tree);

        assert_eq!(code, Some(expected), "{why}: {screen}");
        let pids = world.recorded_pids();
        assert_eq!(pids.len(), 1, "{why}: {pids:?}");
        assert_gone(&pids);
    }
}

#[test]
fn the_build_runs_with_no_compiler_wrapper_and_puts_its_intermediate_files_in_the_target_directory()
{
    // A person's shell exports a cache for the compiler, and a place for it in the tree.
    let world = World::new(false, "exit 3\n");
    let cache = world.tree().join(".cache");
    let world = world
        .with_env("RUSTC_WRAPPER", "sccache")
        .with_env("RUSTC_WORKSPACE_WRAPPER", "sccache")
        .with_env("CARGO_BUILD_RUSTC_WRAPPER", "sccache")
        .with_env("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", "sccache")
        .with_env("CARGO_BUILD_BUILD_DIR", cache.join("build"))
        .with_env("SCCACHE_DIR", cache.join("sccache"));
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert_eq!(
        world.cargo_env(),
        [
            "RUSTC_WRAPPER=".to_owned(),
            "RUSTC_WORKSPACE_WRAPPER=".to_owned(),
            "CARGO_BUILD_RUSTC_WRAPPER=unset".to_owned(),
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER=unset".to_owned(),
            format!("CARGO_BUILD_BUILD_DIR={}", World::canonical(world.target())),
            format!("SCCACHE_DIR={}", cache.join("sccache").display()),
        ],
        "no compiler wrapper runs (the empty string overrides every other setting of one), the \
         intermediate artifacts go where the artifacts go, and the cache of a wrapper that does \
         not run is left as it was set"
    );
    assert!(
        !cache.exists(),
        "nothing was written where the person's shell keeps its compiler cache"
    );
}

#[test]
fn the_program_that_runs_is_a_private_copy_that_the_command_made_in_the_scratch_area() {
    let world = World::new(false, SHOWS_THE_COPY);
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    let copies = world.lines("copies");
    assert!(
        !copies.is_empty(),
        "the program ran, and the area of the build was there"
    );
    for pair in copies.chunks(2) {
        assert!(
            pair[0].contains("/xh-scratch-") && pair[0].ends_with("/excise"),
            "a copy of the binary in the area that the command made for the build: {pair:?}"
        );
        assert_eq!(
            pair.get(1).map(String::as_str),
            Some("same"),
            "the copy is the program that was built: {pair:?}"
        );
    }
    assert!(
        world.target().join("release/excise").is_file(),
        "what cargo built is where cargo put it, and was not what was run"
    );
    assert_eq!(world.scratch_entries(), Vec::<String>::new());
}

#[test]
fn a_binary_that_is_replaced_after_cargo_reported_it_is_not_the_program_that_runs() {
    // Another build in the same target directory replaces `release/excise` once cargo's lock is
    // free, and the soak would open the path later. The command opens the file as cargo reports
    // it, while cargo still runs: the stand-in waits a second after its report, far longer than
    // the thread that reads the report needs, and only then puts another program in the path.
    let world = World::new(false, "exit 3\n");
    world.cargo(
        "install \"$target/release/excise\"\n\
         artifact \"$target/release/excise\"\n\
         sleep 1\n\
         cp '@DIR@/stale-excise' \"$target/release/excise.next\"\n\
         mv \"$target/release/excise.next\" \"$target/release/excise\"\n\
         finished\n",
    );
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert!(
        world.built_excise_started(),
        "the binary that cargo reported was run: {screen}"
    );
    assert!(
        !world.stale_started(),
        "the program that replaced it after the report was run: {screen}"
    );
    assert!(!world.decoy_started());
    assert_eq!(only_run(&world).outcome, SoakOutcome::Finished);
    assert_eq!(world.scratch_entries(), Vec::<String>::new());
}

#[test]
fn a_relative_scratch_directory_is_resolved_where_the_command_runs_and_the_build_gets_it_absolute()
{
    // The command runs in `sub`, and `EXCISE_E2E_TMPDIR=tmp` is `sub/tmp` there. The build runs in
    // the checkout, where `tmp` would be another directory: it is given the absolute path.
    let world = World::new(false, "exit 3\n").with_env("EXCISE_E2E_TMPDIR", "tmp");
    let sub = world.path("sub");
    for directory in [sub.clone(), sub.join("tmp")] {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .expect("a directory where the command runs");
    }
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt_in(&world, sub.clone());
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    let scratch = fs::canonicalize(sub.join("tmp")).expect("the scratch directory");
    let tmpdirs = world.build_tmpdirs();
    let [tmpdir] = tmpdirs.as_slice() else {
        panic!("the build ran once: {tmpdirs:?}");
    };
    assert!(
        Path::new(tmpdir).is_absolute() && Path::new(tmpdir).starts_with(&scratch),
        "the build's temporary directory is below the scratch directory, as an absolute path: \
         {tmpdir}"
    );
    assert_eq!(
        fs::read_dir(sub.join("tmp"))
            .expect("the scratch directory")
            .count(),
        0,
        "nothing was left in it"
    );
}

#[test]
fn the_commit_is_looked_up_in_the_checkout_that_is_built_wherever_the_command_runs() {
    // The command is run from a directory that is not the checkout, as `cargo xtask` from a nested
    // repository would be, and the shell has exported a repository of its own.
    let world = World::new(false, "exit 3\n").with_env("GIT_DIR", "/not/a/repository/.git");
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt_in(&world, world.path("tree"));
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert_eq!(
        world.git_cwds(),
        [fs::canonicalize(repository())
            .expect("the checkout")
            .to_string_lossy()
            .into_owned()],
        "git was run in the checkout that is built"
    );
    assert_eq!(
        world.git_envs(),
        ["unset"],
        "the repository that the shell exported did not reach git"
    );
    assert_eq!(only_run(&world).git_sha, FAKE_SHA);
}

#[test]
fn a_lookup_of_the_commit_that_does_not_end_is_ended_by_the_bound_and_by_a_signal() {
    for (why, signal, said) in [
        (
            "the bound",
            None,
            "the lookup of HEAD passed the bound on the run, so nothing was run",
        ),
        (
            "a signal",
            Some(Signal::Int),
            "the lookup of HEAD was interrupted, so nothing was run",
        ),
    ] {
        let world = World::new(false, "exit 3\n");
        world.script("bin/git", SLEEPS);
        let world = if signal.is_none() {
            world.with_arguments(&["--timeout", "5s"])
        } else {
            world
        };
        let tree = world.tree().to_string_lossy().into_owned();
        let mut session = at_the_prompt(&world);
        session
            .send(format!("{tree}\r").as_bytes())
            .expect("the line is typed");
        if let Some(signal) = signal {
            wait(&mut session, |_| !world.recorded_pids().is_empty());
            send_signal(session.pid(), signal).expect("the signal is delivered");
        }

        let (screen, code) = finish(&mut session);

        assert_eq!(code, Some(1), "{why}: {screen}");
        assert!(screen.contains(said), "{why}: {screen}");
        assert!(
            world.builds().is_empty() && !world.built_excise_started() && !world.decoy_started(),
            "{why}: something was built or started after a lookup that was ended"
        );
        assert!(
            !world.target().join("excise-soak").exists(),
            "{why}: something was written in the output directory"
        );
        let pids = world.recorded_pids();
        assert_eq!(pids.len(), 1, "{why}: {pids:?}");
        assert_gone(&pids);
    }
}

#[test]
fn an_output_path_that_is_not_a_directory_is_refused_before_the_prompt_and_left_alone() {
    let world = World::new(false, "exit 3\n");
    fs::create_dir(world.target()).expect("a target directory");
    let file = world.target().join("excise-soak");
    fs::write(&file, b"a note").expect("a file");

    let mut session = start(&world, None);
    let (screen, code) = finish(&mut session);

    assert_eq!(code, Some(1), "{screen}");
    assert!(
        screen.contains("cannot take a run") && screen.contains("is not a directory"),
        "{screen}"
    );
    assert!(
        !screen.contains("or anything else to stop:"),
        "no prompt is shown: {screen}"
    );
    assert!(
        !world.anything_started(),
        "the build, git, or a program was run"
    );
    assert_eq!(
        fs::read(&file).expect("the file"),
        b"a note",
        "what is in the way was left alone"
    );
}

#[test]
fn a_latest_that_no_run_made_is_refused_before_the_prompt_and_left_alone() {
    for (why, a_file) in [("a file", true), ("a directory", false)] {
        let world = World::new(false, "exit 3\n");
        let output = world.target().join("excise-soak");
        fs::create_dir_all(&output).expect("an output directory");
        let latest = output.join("latest");
        if a_file {
            fs::write(&latest, b"draft").expect("a file");
        } else {
            fs::create_dir(&latest).expect("a directory");
        }

        let mut session = start(&world, None);
        let (screen, code) = finish(&mut session);

        assert_eq!(code, Some(1), "{why}: {screen}");
        assert!(
            screen.contains("cannot take a run")
                && screen.contains("is not a link that a run made"),
            "{why}: {screen}"
        );
        assert!(
            !screen.contains("or anything else to stop:"),
            "{why}: no prompt is shown: {screen}"
        );
        assert!(
            !world.anything_started(),
            "{why}: the build, git, or a program was run"
        );
        assert_eq!(
            fs::read_dir(&output).expect("the directory").count(),
            1,
            "{why}: nothing was added to the output directory"
        );
        if a_file {
            assert_eq!(fs::read(&latest).expect("the file"), b"draft", "{why}");
        } else {
            assert!(latest.is_dir(), "{why}");
        }
    }
}

#[test]
fn git_is_given_no_variable_of_its_own_so_a_trace_destination_in_the_root_creates_nothing() {
    // Git writes a trace to the file that `GIT_TRACE` and `GIT_TRACE2_EVENT` name, and the shell
    // can name one in the tree. The stand-in for `git` does what git does with `GIT_TRACE`.
    let world = World::new(false, "exit 3\n");
    let trace = world.tree().join("git-trace.log");
    let events = world.tree().join("git-trace2-events.json");
    let world = world
        .with_env("GIT_TRACE", trace.clone())
        .with_env("GIT_TRACE2_EVENT", events.clone())
        .with_env("GIT_DIR", "/not/a/repository/.git")
        .with_env("GIT_CONFIG_COUNT", "0");
    let tree = world.tree().to_string_lossy().into_owned();
    let before = snapshot(&world.tree());

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert_eq!(
        world.git_runs(),
        ["rev-parse HEAD"],
        "git ran, so what follows is not vacuous"
    );
    assert_eq!(
        world.git_vars(),
        Vec::<String>::new(),
        "no variable of git's own reached it"
    );
    assert!(
        !trace.exists() && !events.exists(),
        "git wrote a trace in the root"
    );
    assert_eq!(
        snapshot(&world.tree()),
        before,
        "the root is byte for byte as it was"
    );
}

#[test]
fn a_link_to_the_target_directory_that_is_retargeted_after_the_prompt_is_not_followed() {
    let world = World::new(false, "exit 3\n");
    let real = world.path("real-target");
    let decoy = world.tree().join("decoy-target");
    fs::create_dir(&real).expect("a directory outside the root");
    fs::create_dir(&decoy).expect("a directory in the root");
    let link = world.path("link-to-the-target");
    symlink(&real, &link).expect("a link");
    let world = world.with_target(link.clone());
    let tree = world.tree().to_string_lossy().into_owned();
    let before = snapshot(&world.tree());

    let mut session = at_the_prompt(&world);
    let prompt = flat(&session);
    // The prompt has named where the build goes. Now the link is moved into the root.
    fs::remove_file(&link).expect("the link goes");
    symlink(&decoy, &link).expect("the link leads into the root");
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert!(
        prompt.contains("write nothing in that directory")
            && prompt.contains(&World::canonical(&real)),
        "the prompt named the real directory, outside the root: {prompt}"
    );
    assert_eq!(
        world.builds(),
        [format!(
            "build --release --locked --offline -p excise --target-dir {} \
             --message-format json-render-diagnostics",
            World::canonical(&real)
        )],
        "the build is told the directory that the prompt named, and not the link"
    );
    assert!(
        real.join("release/excise").is_file(),
        "what was built is where the prompt said"
    );
    assert!(
        real.join("excise-soak").is_dir(),
        "and so are the soak's files"
    );
    assert_eq!(
        snapshot(&world.tree()),
        before,
        "nothing was written in the root, through the link or otherwise"
    );
}

#[test]
fn a_link_to_cargos_home_that_is_retargeted_after_the_prompt_is_not_followed() {
    let world = World::new(false, "exit 3\n");
    let real = world.path("real-cargo-home");
    let decoy = world.tree().join("decoy-cargo-home");
    fs::create_dir(&real).expect("a directory outside the root");
    fs::create_dir(&decoy).expect("a directory in the root");
    let link = world.path("link-to-cargo-home");
    symlink(&real, &link).expect("a link");
    let world = world.with_env("CARGO_HOME", link.clone());
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let prompt = flat(&session);
    // The prompt has judged cargo's home to lie outside the root. Now the link is moved into it.
    fs::remove_file(&link).expect("the link goes");
    symlink(&decoy, &link).expect("the link leads into the root");
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert!(
        prompt.contains("write nothing in that directory") && !prompt.contains(".global-cache"),
        "cargo's home was outside the root when the prompt was shown: {prompt}"
    );
    assert_eq!(
        world.build_cargo_homes(),
        [World::canonical(&real)],
        "the build is given the directory that the prompt classified, and not the link"
    );
}

#[test]
fn a_scratch_area_that_cannot_be_removed_is_an_error_named_after_the_report_is_printed() {
    // A process that is root can remove what it likes, and there is nothing to try.
    let probe = tempfile::Builder::new()
        .prefix("xt-xtask-probe-")
        .tempdir()
        .expect("a directory");
    let locked_probe = probe.path().join("locked");
    fs::create_dir(&locked_probe).expect("a directory");
    fs::write(locked_probe.join("file"), b"x").expect("a file");
    fs::set_permissions(&locked_probe, fs::Permissions::from_mode(0o500)).expect("chmod");
    let removable = fs::remove_file(locked_probe.join("file")).is_ok();
    fs::set_permissions(&locked_probe, fs::Permissions::from_mode(0o700)).expect("chmod");
    if removable {
        eprintln!("skipped: a process that is root can remove what it likes");
        return;
    }

    // The build leaves a directory that cannot be written in its temporary directory, as a
    // compiler or a build script can.
    let world = World::new(false, "exit 3\n");
    world.cargo(&format!(
        "mkdir \"$TMPDIR/locked\"\n\
         echo x > \"$TMPDIR/locked/file\"\n\
         chmod 555 \"$TMPDIR/locked\"\n\
         {BUILDS}"
    ));
    let _unlocks = RemovesTheScratchArea(&world);
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(1), "{screen}");
    let entries = world.scratch_entries();
    let [left] = entries.as_slice() else {
        panic!("the area that could not be removed is the one entry left: {entries:?}");
    };
    let area = fs::canonicalize(world.path("scratch"))
        .expect("the scratch directory")
        .join(left);
    let reported = screen.find("summary.json").expect("the report is printed");
    let removal = screen
        .find("could not be removed")
        .expect("the removal is said");
    assert!(
        reported < removal,
        "the report comes first, and then the error: {screen}"
    );
    assert!(
        screen.contains(&format!(
            "the scratch area of the build, {} could not be removed (permission denied)",
            safe_path_text(&area)
        )),
        "it names the area and says why: {screen}"
    );
    assert_eq!(
        only_run(&world).outcome,
        SoakOutcome::Finished,
        "the run itself went well, and what it found is recorded"
    );
}

/// Removes what a build left in the scratch directory of a world, when it is dropped, though a
/// directory in it cannot be written: the world's own clean-up could not.
struct RemovesTheScratchArea<'a>(&'a World);

impl Drop for RemovesTheScratchArea<'_> {
    fn drop(&mut self) {
        let scratch = self.0.path("scratch");
        let Ok(entries) = fs::read_dir(&scratch) else {
            return;
        };
        for entry in entries.flatten() {
            let _ = Command::new("chmod")
                .args(["-R", "u+rwx"])
                .arg(entry.path())
                .status();
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

#[test]
fn a_build_that_writes_a_line_of_megabytes_still_reports_the_binary_it_made() {
    // A build script can make cargo write a line of any length. The command reads no more of it
    // than a limit, drops the rest, and goes on to the line that reports the binary.
    let world = World::new(false, "exit 3\n");
    world.cargo(&format!(
        "head -c 5000000 /dev/zero | tr '\\0' x\n\
         echo\n\
         {BUILDS}"
    ));
    let tree = world.tree().to_string_lossy().into_owned();

    let mut session = at_the_prompt(&world);
    let (screen, code) = type_and_finish(&mut session, &tree);

    assert_eq!(code, Some(0), "{screen}");
    assert!(
        world.built_excise_started(),
        "the binary that cargo reported after the long line was run: {screen}"
    );
    assert_eq!(only_run(&world).outcome, SoakOutcome::Finished);
}
