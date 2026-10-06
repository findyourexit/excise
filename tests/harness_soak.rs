//! A read-only soak of a generated fixture, run against this crate's own `excise` binary: headless
//! and in a pseudo-terminal, as `cargo xtask soak` runs them on a real tree.
//!
//! The fixture is a tree of distinctive names that the test generates itself. The soak must finish,
//! write a document that validates against its schema, leave the tree byte for byte as it was, and
//! put no name and no path from the tree in `summary.json`, which is made to be shared.

use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use excise_harness::{
    fixture::{FixtureCache, Fixtures},
    report::{Document as _, HarnessSoak, QuirkKind, SoakOutcome, tui::ExitVia},
    runner::work_base,
    safety::{FixtureSnapshot, check_private_directory},
    scenario::Profile,
    soak::{Interrupt, Limits, SoakRequest, SoakRoot, run_soak},
};
use serde_json::Value;

/// A fixture spec whose names say what they are: nothing in them may reach `summary.json`. The
/// archive is the largest folder, so the session opens it.
const SPEC: &str = r#"schema_version = 1
id = "soak-names"
description = "Distinctive names, so that a test can tell whether any of them reached a document."
seed = 20261006

[[parts]]
kind = "file"
root = "Zebra-Quarterly-Ledger-7731.dat"
size = 8192

[[parts]]
kind = "tree"
root = "Aardvark Customer Archive"
depth = 0
files_per_dir = 8
file_names = { style = "sequential", prefix = "Confidential-Payroll-", suffix = ".xlsx" }
file_size = 262144

[[parts]]
kind = "tree"
root = "Mongoose Holiday Photos"
depth = 0
files_per_dir = 3
file_names = { style = "sequential", prefix = "Private-Wedding-", suffix = ".jpg" }
file_size = 8192
"#;

/// A tree with folders that its owner cannot read, which is how a real home directory ends a scan:
/// not `COMPLETE`, but with a label of an uncertain result. The archive is the largest entry, and
/// a folder.
#[cfg(unix)]
const UNREADABLE_SPEC: &str = r#"schema_version = 1
id = "soak-unreadable"
description = "A tree with folders that cannot be read, and an archive that is its largest folder."
seed = 20261007

[[parts]]
kind = "tree"
root = "Aardvark Customer Archive"
depth = 0
files_per_dir = 8
file_names = { style = "sequential", prefix = "Confidential-Payroll-", suffix = ".xlsx" }
file_size = 262144

[[parts]]
kind = "hostile"
root = "hostile"
features = ["unreadable_dirs"]
"#;

/// A tree whose largest entry is a file: the cursor starts on a file, and the soak has to move it
/// to a folder to drill.
#[cfg(unix)]
const FILE_FIRST_SPEC: &str = r#"schema_version = 1
id = "soak-big-file"
description = "A tree whose largest entry is a file, beside two folders."
seed = 20261008

[[parts]]
kind = "file"
root = "Zebra-Backup-Image.img"
size = 4194304

[[parts]]
kind = "tree"
root = "Aardvark Customer Archive"
depth = 0
files_per_dir = 4
file_names = { style = "sequential", prefix = "Confidential-Payroll-", suffix = ".xlsx" }
file_size = 262144

[[parts]]
kind = "tree"
root = "Mongoose Holiday Photos"
depth = 0
files_per_dir = 3
file_names = { style = "sequential", prefix = "Private-Wedding-", suffix = ".jpg" }
file_size = 65536
"#;

/// What one soak of the fixture left to look at.
struct Soaked {
    document: HarnessSoak,
    summary: String,
    quirks: String,
    /// Every file's bytes and every directory, by path below the root, before and after.
    before: BTreeMap<String, Option<Vec<u8>>>,
    after: BTreeMap<String, Option<Vec<u8>>>,
    /// Whether the structural snapshot of the harness saw any difference. A tree with a folder
    /// that cannot be listed has no snapshot, and its bytes are compared instead.
    snapshot_changed: bool,
    /// Every name in the tree, and the paths the soak worked with: none may be in `summary`.
    forbidden: Vec<String>,
    /// What was left in the scratch parent.
    scratch_left: Vec<String>,
}

/// Every directory (as `None`) and file (as its bytes) below `root`, by relative path. A directory
/// that cannot be listed is a directory, and nothing below it is known.
fn contents(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    fn walk(dir: &Path, relative: &str, into: &mut BTreeMap<String, Option<Vec<u8>>>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries {
            let entry = entry.expect("an entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            let key = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let kind = entry.file_type().expect("a file type");
            if kind.is_dir() {
                into.insert(key.clone(), None);
                walk(&entry.path(), &key, into);
            } else {
                into.insert(key, Some(fs::read(entry.path()).unwrap_or_default()));
            }
        }
    }
    let mut into = BTreeMap::new();
    walk(root, "", &mut into);
    into
}

/// Makes a directory that only its owner can write, whatever the umask is: the soak refuses a
/// scratch directory that a group or everybody can write.
fn private_directory(path: &Path) {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;

        builder.mode(0o700);
    }
    builder.create(path).expect("a private directory");
}

/// The places this test may make its work area in, best first: the harness's own base (`/tmp`, or
/// `EXCISE_E2E_TMPDIR`), the directory that cargo gives integration tests for their files, and the
/// system's temporary directory.
fn candidate_bases() -> Vec<PathBuf> {
    vec![
        work_base(),
        PathBuf::from(env!("CARGO_TARGET_TMPDIR")),
        env::temp_dir(),
    ]
}

/// The first of `candidates` that `accepts`, or every refusal, one to a line, when none does.
fn first_accepted(
    candidates: &[PathBuf],
    accepts: impl Fn(&Path) -> Result<(), String>,
) -> Result<&Path, String> {
    let mut refusals = Vec::new();
    for candidate in candidates {
        match accepts(candidate) {
            Ok(()) => return Ok(candidate),
            Err(why) => refusals.push(format!("{}: {why}", candidate.display())),
        }
    }
    Err(refusals.join("\n"))
}

/// A new work area below the first base that the soak accepts as the place to run a copy of the
/// program from, or `None` where the environment has none.
///
/// The soak refuses a scratch directory that another user can change: it, and every directory above
/// it, must be on a file system that enforces ownership, be owned by the person or by root, and be
/// closed to the group and to everybody unless it is sticky. That is a rule about the place and not
/// about the test, and some build environments have no place that passes it. A Nix build sandbox
/// shows its `/tmp` as owned by `nobody` (user 65534), because root, which made it, is not a user
/// that the sandbox maps, and with the usual single build user the root of its file system is
/// root's too, so no directory below them is one that only the person who builds can change. A run
/// there says so and goes without the soak, as the `skipped:` lines of other tests do; the rule is
/// not bent for it, and the refusal itself is tested in the library. A run in CI (the variable
/// `CI`, which every native job has and the Nix sandbox does not) must have a place, so that an
/// environment that lost its own cannot skip these tests without anybody seeing.
fn work_area() -> Option<tempfile::TempDir> {
    let candidates = candidate_bases();
    // Cargo makes its directory for integration tests, and a copy of the test elsewhere may not.
    let _ = fs::create_dir_all(env!("CARGO_TARGET_TMPDIR"));
    let accepts = |base: &Path| {
        check_private_directory(base).map_err(|refusal| format!("{refusal}; {}", refusal.way_out()))
    };
    match first_accepted(&candidates, accepts) {
        Ok(base) => Some(
            tempfile::Builder::new()
                .prefix("xt-soak-")
                .tempdir_in(base)
                .expect("a work area"),
        ),
        Err(refusals) => {
            let why = format!(
                "no directory here is one that the soak accepts as a scratch directory:\n{refusals}"
            );
            assert!(env::var_os("CI").is_none(), "{why}");
            eprintln!("skipped: {why}");
            None
        }
    }
}

/// Generates the fixture `id`, which `spec` describes, and soaks it once, headless and in a
/// terminal. `None` where the environment has no place for the soak's scratch areas ([`work_area`]).
fn soak_a_generated_fixture(id: &str, spec: &str) -> Option<Soaked> {
    let work = work_area()?;
    let specs = work.path().join("specs");
    fs::create_dir_all(&specs).expect("a directory of specs");
    fs::write(specs.join(format!("{id}.toml")), spec).expect("a spec");
    let fixtures = Fixtures::new(specs, FixtureCache::at(work.path().join("cache")));
    let parent = work.path().join("fixture");
    fs::create_dir_all(&parent).expect("a parent for the fixture");
    let copy = fixtures
        .run_copy(id, &parent)
        .expect("the fixture is generated");
    let root_path = copy.root().to_path_buf();

    let snapshot = FixtureSnapshot::take(&root_path).ok();
    let before = contents(&root_path);
    let mut forbidden: Vec<String> = before
        .keys()
        .flat_map(|path| path.split('/').map(str::to_owned).collect::<Vec<_>>())
        .collect();

    let binary = PathBuf::from(env!("CARGO_BIN_EXE_excise"));
    let scratch = work.path().join("scratch");
    private_directory(&scratch);
    forbidden.extend([
        root_path.to_string_lossy().into_owned(),
        work.path().to_string_lossy().into_owned(),
        binary.to_string_lossy().into_owned(),
    ]);
    let root = SoakRoot::open(&root_path).expect("the fixture is a directory");
    let report = run_soak(
        &SoakRequest {
            root: &root,
            binary: &binary,
            binary_identity: None,
            out_root: &work.path().join("out"),
            work_dir: &scratch,
            rounds: 1,
            limits: Limits::for_run(Duration::from_mins(5)),
            started: Instant::now(),
            record: false,
            git_sha: &"0".repeat(40),
            interrupt: &Interrupt::new(),
        },
        &mut |_| {},
    )
    .unwrap_or_else(|error| panic!("the soak runs: {error}"));
    assert_eq!(report.failure, None);

    let snapshot_changed = snapshot.is_some_and(|taken| {
        !FixtureSnapshot::take(&root_path).is_ok_and(|after| taken.diff(&after).is_empty())
    });
    Some(Soaked {
        summary: fs::read_to_string(report.run_dir.join("summary.json")).expect("summary.json"),
        quirks: fs::read_to_string(report.run_dir.join("quirks.txt")).expect("quirks.txt"),
        document: report.document,
        before,
        after: contents(&root_path),
        snapshot_changed,
        forbidden,
        scratch_left: fs::read_dir(&scratch)
            .expect("the scratch parent")
            .map(|entry| {
                entry
                    .expect("an entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect(),
    })
}

#[test]
fn a_full_soak_of_a_generated_fixture_finishes_validates_and_changes_nothing() {
    let Some(soaked) = soak_a_generated_fixture("soak-names", SPEC) else {
        return;
    };
    let document = &soaked.document;

    assert_eq!(document.outcome, SoakOutcome::Finished, "{}", soaked.quirks);
    assert_eq!(document.rounds.completed, 1);

    // The headless scan: it ran, exited 0 on an exact tree, and its report held counts only.
    let [scan] = document.headless.as_slice() else {
        panic!("one headless scan: {:?}", document.headless);
    };
    assert_eq!(scan.exit_code, Some(0), "{}", soaked.quirks);
    assert!(!scan.timed_out && scan.residue_files == 0 && scan.wall_ms > 0.0);
    let facts = scan.report.expect("the scan's report was read");
    assert!(facts.summary.scanned_entries >= 12, "{facts:?}");
    assert_eq!(facts.summary.deleted_entries, 0);
    assert_eq!(facts.summary.unreadable_entries, 0);

    // The sessions: one per profile, each quit the way a user does, nothing left behind.
    let profiles: Vec<Profile> = document.tui.iter().map(|session| session.profile).collect();
    assert_eq!(profiles, [Profile::Default, Profile::Deterministic]);
    for session in &document.tui {
        let name = session.profile;
        assert_eq!(session.exit.via, ExitVia::Quit, "{name}: {}", soaked.quirks);
        assert_eq!(session.exit.code, Some(0), "{name}");
        assert_eq!(session.timed_out_phase, None, "{name}");
        assert!(
            session.terminal_restored && session.residue_files == 0,
            "{name}"
        );
        for metric in [
            "first_frame_ms",
            "complete_ms",
            "duration_ms",
            "inputs_sent",
        ] {
            assert!(session.metrics.contains_key(metric), "{name}: {metric}");
        }
        // Every key the soak sent is counted, not only the probes timed during the scan: at the
        // least `Enter`, `Esc`, `q`, and `y`.
        assert!(session.metrics["inputs_sent"] >= 4.0, "{name}");
        if cfg!(unix) {
            assert!(session.drilled, "{name}: {}", soaked.quirks);
            assert!(
                session.metrics.contains_key("drill_ms") && session.metrics.contains_key("up_ms")
            );
        }
    }

    // The quirks a soak of a plain tree may note are timing ones; anything else is a finding.
    let unexpected: Vec<_> = document
        .quirks
        .iter()
        .filter(|quirk| !matches!(quirk.kind, QuirkKind::Stall | QuirkKind::NoFrameForKey))
        .collect();
    assert!(unexpected.is_empty(), "{unexpected:?}\n{}", soaked.quirks);

    // What was written is what the schema says, read back through the strict reader as well.
    assert_eq!(
        HarnessSoak::from_json_str(&soaked.summary).expect("a document that validates"),
        *document
    );
    let schema: Value = serde_json::from_str(HarnessSoak::SCHEMA_JSON).expect("the schema is JSON");
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("the schema compiles");
    let written: Value = serde_json::from_str(&soaked.summary).expect("summary.json is JSON");
    assert!(validator.is_valid(&written), "{}", soaked.summary);

    // Read-only: every directory and every byte is as it was, and nothing is left in the scratch
    // area the soak was given.
    assert!(
        !soaked.snapshot_changed,
        "the snapshot of the fixture changed"
    );
    assert_eq!(
        soaked.before, soaked.after,
        "the tree is not byte for byte as it was"
    );
    assert!(soaked.before.len() >= 12, "{:?}", soaked.before.keys());
    assert!(soaked.scratch_left.is_empty(), "{:?}", soaked.scratch_left);
}

#[test]
fn summary_json_holds_no_name_and_no_path_from_the_tree() {
    let Some(soaked) = soak_a_generated_fixture("soak-names", SPEC) else {
        return;
    };

    assert!(
        soaked
            .forbidden
            .iter()
            .any(|name| name.contains("Confidential-Payroll-"))
    );
    for name in soaked.forbidden.iter().filter(|name| name.len() >= 5) {
        assert!(
            !soaked.summary.contains(name.as_str()),
            "summary.json holds `{name}`"
        );
    }
    assert!(
        !soaked.summary.contains('/') && !soaked.summary.contains('\\'),
        "summary.json holds a path separator: {}",
        soaked.summary
    );
}

/// Whether this user is stopped by a directory's mode. Root is not, and neither is a file system
/// that ignores modes, and there a folder that cannot be listed does not exist.
#[cfg(unix)]
fn modes_are_enforced() -> bool {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a directory");
    let locked = dir.path().join("locked");
    fs::create_dir(&locked).expect("a directory");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("a mode");
    let enforced = fs::read_dir(&locked).is_err();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("restored");
    enforced
}

#[cfg(unix)]
#[test]
fn a_tree_with_folders_that_cannot_be_read_is_soaked_to_the_end_and_its_uncertainty_is_a_quirk() {
    if !modes_are_enforced() {
        eprintln!("skipped: this user can list a directory whose mode is 000");
        return;
    }
    let Some(soaked) = soak_a_generated_fixture("soak-unreadable", UNREADABLE_SPEC) else {
        return;
    };
    let document = &soaked.document;

    // A scan that is uncertain is a scan, and a soak gates nothing: the run ends in the ordinary
    // way, and each part of it is there.
    assert_eq!(document.outcome, SoakOutcome::Finished, "{}", soaked.quirks);
    assert_eq!(document.rounds.completed, 1);
    let [scan] = document.headless.as_slice() else {
        panic!("one headless scan: {:?}", document.headless);
    };
    let facts = scan.report.expect("the scan's report was read");
    assert!(facts.summary.unreadable_entries >= 1, "{facts:?}");

    // Both sessions: the scan ended with a label of uncertainty and not `COMPLETE`, and the
    // session went on to open the largest folder, leave it, and quit the way a user does.
    let profiles: Vec<Profile> = document.tui.iter().map(|session| session.profile).collect();
    assert_eq!(profiles, [Profile::Default, Profile::Deterministic]);
    for session in &document.tui {
        let name = session.profile;
        assert_eq!(session.timed_out_phase, None, "{name}: {}", soaked.quirks);
        assert_eq!(session.exit.via, ExitVia::Quit, "{name}: {}", soaked.quirks);
        assert!(session.drilled, "{name}: {}", soaked.quirks);
        assert!(
            session.terminal_restored && session.residue_files == 0,
            "{name}"
        );
        for metric in ["complete_ms", "drill_ms", "up_ms"] {
            assert!(session.metrics.contains_key(metric), "{name}: {metric}");
        }
    }

    // The uncertainty is a quirk counted by kind in the document; the label itself is text, and is
    // in the local log alone.
    let uncertain = document
        .quirks
        .iter()
        .find(|quirk| quirk.kind == QuirkKind::UncertainScan)
        .unwrap_or_else(|| panic!("no uncertain scan was noted:\n{}", soaked.quirks));
    assert!(uncertain.count >= 2, "{uncertain:?}\n{}", soaked.quirks);
    for profile in ["default", "deterministic"] {
        assert!(
            soaked.quirks.contains(&format!(
                "round 1, tui {profile}: the scan ended with the header `"
            )),
            "{profile}:\n{}",
            soaked.quirks
        );
    }
    assert!(
        !soaked.summary.contains("NEEDS REVIEW") && !soaked.summary.contains("READ ERROR"),
        "the label is text, and summary.json holds none: {}",
        soaked.summary
    );

    // Nothing went wrong that a quirk of another kind would say.
    let wrong: Vec<_> = document
        .quirks
        .iter()
        .filter(|quirk| {
            matches!(
                quirk.kind,
                QuirkKind::Timeout
                    | QuirkKind::ExitedEarly
                    | QuirkKind::QuitRefused
                    | QuirkKind::DrillSkipped
                    | QuirkKind::TerminalNotRestored
                    | QuirkKind::Residue
                    | QuirkKind::HarnessError
            )
        })
        .collect();
    assert!(wrong.is_empty(), "{wrong:?}\n{}", soaked.quirks);

    // The document validates, and the tree is as it was.
    assert_eq!(
        HarnessSoak::from_json_str(&soaked.summary).expect("a document that validates"),
        *document
    );
    assert_eq!(soaked.before, soaked.after, "the tree changed");
    assert!(soaked.scratch_left.is_empty(), "{:?}", soaked.scratch_left);
}

#[cfg(unix)]
#[test]
fn a_tree_whose_largest_entry_is_a_file_still_has_a_folder_opened() {
    let Some(soaked) = soak_a_generated_fixture("soak-big-file", FILE_FIRST_SPEC) else {
        return;
    };
    let document = &soaked.document;

    assert_eq!(document.outcome, SoakOutcome::Finished, "{}", soaked.quirks);
    for session in &document.tui {
        let name = session.profile;
        assert_eq!(session.timed_out_phase, None, "{name}: {}", soaked.quirks);
        assert_eq!(session.exit.via, ExitVia::Quit, "{name}: {}", soaked.quirks);
        // The cursor starts on the 4 MiB file, which is not a folder: the soak walked it to one
        // with the arrow keys, opened it, and came back.
        assert!(session.drilled, "{name}: {}", soaked.quirks);
        assert!(
            session.metrics.contains_key("drill_ms") && session.metrics.contains_key("up_ms"),
            "{name}"
        );
        // At least one arrow, besides `Enter`, `Esc`, `q`, and `y`.
        assert!(
            session.metrics["inputs_sent"] >= 5.0,
            "{name}: {:?}",
            session.metrics
        );
    }
    assert!(
        !document
            .quirks
            .iter()
            .any(|quirk| quirk.kind == QuirkKind::DrillSkipped),
        "{}",
        soaked.quirks
    );
    assert_eq!(soaked.before, soaked.after, "the tree changed");
    assert!(!soaked.snapshot_changed);
    assert!(soaked.scratch_left.is_empty(), "{:?}", soaked.scratch_left);
}

#[test]
fn the_first_base_the_soak_accepts_is_used_and_when_none_is_every_refusal_is_given() {
    let bases = [
        PathBuf::from("/a"),
        PathBuf::from("/b"),
        PathBuf::from("/c"),
    ];
    let refusing = |refused: &'static [&'static str]| {
        move |base: &Path| -> Result<(), String> {
            if refused.iter().any(|name| base == Path::new(name)) {
                Err(format!("`{}` is not private", base.display()))
            } else {
                Ok(())
            }
        }
    };

    // The first is used when it is accepted, a refused one falls through to the next, and each
    // is asked in turn.
    assert_eq!(
        first_accepted(&bases, refusing(&[])),
        Ok::<_, String>(Path::new("/a"))
    );
    assert_eq!(
        first_accepted(&bases, refusing(&["/a"])),
        Ok::<_, String>(Path::new("/b"))
    );
    assert_eq!(
        first_accepted(&bases, refusing(&["/a", "/b"])),
        Ok::<_, String>(Path::new("/c"))
    );
    // When none is accepted, every refusal is kept, one to a line, with the base it is of.
    let none =
        first_accepted(&bases, refusing(&["/a", "/b", "/c"])).expect_err("every base is refused");
    assert_eq!(none.lines().count(), 3, "{none}");
    assert!(
        none.lines().all(|line| line.contains("is not private")),
        "{none}"
    );
    assert!(none.contains("/a: ") && none.contains("/c: "), "{none}");
}

#[cfg(unix)]
#[test]
fn a_base_below_a_directory_that_others_can_change_is_still_refused_whatever_is_above_it() {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};

    // The shape of a build sandbox whose `/tmp` belongs to another user: the base is the person's
    // own and closed to everybody, and a directory above it decides. The rule is not bent for it.
    let parent = tempfile::Builder::new()
        .prefix("xt-soak-bases-")
        .tempdir()
        .expect("a directory");
    let make = |path: &Path, mode: u32| {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .expect("a directory");
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("its mode");
    };
    let open = parent.path().join("open");
    make(&open, 0o777);
    let below = open.join("base");
    make(&below, 0o700);
    let private = parent.path().join("private");
    make(&private, 0o700);
    let accepts =
        |base: &Path| check_private_directory(base).map_err(|refusal| refusal.to_string());

    let refused = first_accepted(std::slice::from_ref(&below), accepts)
        .expect_err("a base below an open directory is refused");

    let above = fs::canonicalize(&open).expect("a canonical path");
    assert!(
        refused.contains("can be written by everybody")
            && refused.contains(&*above.to_string_lossy()),
        "it is the directory above the base that is named: {refused}"
    );
    // With that base first and a private one after it, the private one is used, where the
    // environment has a private base at all (a Nix build sandbox does not).
    if check_private_directory(&private).is_ok() {
        let bases = [below, private.clone()];
        assert_eq!(
            first_accepted(&bases, accepts),
            Ok::<_, String>(private.as_path())
        );
    } else {
        eprintln!("skipped: this environment has no private base to fall back to");
    }
}
