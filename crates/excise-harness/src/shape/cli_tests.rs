//! The command line of `excise-shape`: what it accepts, what it writes, where it writes it, and
//! what it refuses. Each test runs the command line in this process, on scratch trees.

use std::{ffi::OsString, fs, io, path::Path};

use super::{
    HANDLE_BUDGET, WalkError, WalkOptions,
    cli::{self, Command, DEFAULT_SEED, FAILURE, Help, SUCCESS, USAGE, UsageError},
    profile,
    tests::Scratch,
};
use crate::{
    fixture::{FixtureSpec, spec::DEFAULT_MAX_FILE_BYTES},
    report::{Document, HarnessShapeProfile},
};

/// Runs a command line, and returns its exit status, standard output, and standard error.
fn run(args: &[&str]) -> (u8, String, String) {
    run_os(args.iter().map(OsString::from))
}

fn run_os(args: impl IntoIterator<Item = OsString>) -> (u8, String, String) {
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let status = cli::status(args, &mut stdout, &mut stderr);
    (
        status,
        String::from_utf8(stdout).expect("standard output is text"),
        String::from_utf8(stderr).expect("standard error is text"),
    )
}

fn parsed(args: &[&str]) -> Result<Command, UsageError> {
    cli::parse(args.iter().map(OsString::from))
}

/// A tree of a few entries, made with `std`.
fn small_tree(scratch: &Scratch) -> std::path::PathBuf {
    let root = scratch.join("tree");
    fs::create_dir_all(root.join("alpha").join("beta")).expect("folders");
    fs::write(root.join("alpha").join("one.txt"), b"1").expect("a file");
    fs::write(root.join("alpha").join("beta").join("two.txt"), b"22").expect("a file");
    fs::write(root.join("three.txt"), b"333").expect("a file");
    root
}

#[test]
fn the_command_line_takes_the_forms_the_help_describes() {
    assert_eq!(
        parsed(&["profile", "/some/root"]).expect("valid"),
        Command::Profile {
            root: "/some/root".into(),
            output: None,
            cross_filesystems: false,
        }
    );
    assert_eq!(
        parsed(&[
            "profile",
            "--cross-filesystems",
            "--output",
            "out.json",
            "/some/root"
        ])
        .expect("valid"),
        Command::Profile {
            root: "/some/root".into(),
            output: Some("out.json".into()),
            cross_filesystems: true,
        }
    );
    let Command::Spec {
        profile,
        request,
        output,
    } = parsed(&[
        "spec",
        "home.json",
        "--id",
        "home-50k",
        "--entries",
        "50000",
    ])
    .expect("valid")
    else {
        panic!("a spec command");
    };
    assert_eq!(profile, Path::new("home.json"));
    assert_eq!(output, None);
    assert_eq!(request.id, "home-50k");
    assert_eq!(request.entries, 50_000);
    assert_eq!(request.seed, DEFAULT_SEED, "the seed is 1 unless given");
    assert_eq!(request.max_file_bytes, DEFAULT_MAX_FILE_BYTES);
    let Command::Spec {
        request, output, ..
    } = parsed(&[
        "spec",
        "home.json",
        "--id",
        "x",
        "--entries",
        "9",
        "--seed",
        "12",
        "--max-file-bytes",
        "100",
        "--output",
        "x.toml",
    ])
    .expect("valid")
    else {
        panic!("a spec command");
    };
    assert_eq!((request.seed, request.max_file_bytes), (12, 100));
    assert_eq!(output.as_deref(), Some(Path::new("x.toml")));
}

#[test]
fn help_is_asked_for_in_every_place_it_can_be() {
    for (args, expected) in [
        (&["--help"][..], Help::General),
        (&["-h"], Help::General),
        (&["help"], Help::General),
        (&["help", "profile"], Help::Profile),
        (&["profile", "--help"], Help::Profile),
        (&["profile", "some-root", "-h"], Help::Profile),
        (&["help", "spec"], Help::Spec),
        (&["spec", "--help"], Help::Spec),
    ] {
        assert_eq!(
            parsed(args).expect("valid"),
            Command::Help(expected),
            "{args:?}"
        );
        let (status, stdout, stderr) = run(args);
        assert_eq!(status, SUCCESS);
        assert_eq!(stdout, expected.text());
        assert!(stderr.is_empty());
    }
    // What the help says it does and where the result goes, with its lines joined: a promise is
    // not broken by where a line wraps.
    let profile = Help::Profile.text();
    let flat = profile.split_whitespace().collect::<Vec<_>>().join(" ");
    let budget = format!("at most {HANDLE_BUDGET} folder handles open at once");
    for promise in [
        "aggregates only",
        "no name, no path, no link target, no owner, and no timestamp",
        "No message of this command names ROOT or anything below it",
        "(`lstat`)",
        "it writes nothing, and --output makes its one new file after the walk has ended",
        "wherever FILE is, ROOT included",
        "is resolved like any path you type, and a link among its components is followed",
        "its last component must be a folder itself, not a link to one",
        "Nothing below ROOT is followed",
        "`link/` and `link/.` are `link`, and a link is refused written either way",
        "the walk asks nothing of where it points, not even whether its target exists",
        "in memory only (nothing goes to disk)",
        "the names of the folder it is listing, all at once",
        "the names of the subfolders it has still to visit, in each folder on the path from ROOT to \
         the folder it is in",
        "the identity of every file that has more than one name, until the walk ends",
        "Its memory grows with those three and with nothing else of the tree: with the widest \
         folder, with the subfolders waiting along a path (in a chain of folders that each hold \
         many subfolders, they can approach the number of folders in the tree), and with the \
         files that have more than one name (at most every file)",
        "A tree for which any of them does not fit in memory cannot be profiled",
        "--cross-filesystems",
        budget.as_str(),
        "each one that is missing is made on its own",
        "readable and writable by its owner only (mode 0600)",
        "on Windows the file and those folders get the permissions of the folder they are made in",
        "stays private to you",
        "target/excise-profiles/",
    ] {
        assert!(
            flat.contains(promise),
            "`excise-shape help profile` says `{promise}`"
        );
    }
    // What it used to say, and was not true: the walk makes no file below ROOT, but `--output` can.
    // And what it said of links before it stopped asking about their targets.
    for retracted in [
        "writes nothing below ROOT",
        "never follows a symbolic link",
        "a single `stat`",
        "links that dangle",
        "memory grows with the widest folder",
        "never grows with the size of the tree",
    ] {
        assert!(
            !flat.contains(retracted),
            "`excise-shape help profile` no longer says `{retracted}`"
        );
    }
    let spec = Help::Spec
        .text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(spec.contains("--fixture-dir"));
    assert!(
        spec.contains("as `excise-shape profile --output` writes its file"),
        "a spec is written as a profile is: {spec}"
    );
}

#[test]
fn a_command_line_that_does_not_say_what_to_do_is_a_usage_error() {
    let cases: [(&[&str], &str); 17] = [
        (&[], "name a command"),
        (&["frobnicate"], "there is no command `frobnicate`"),
        (&["help", "frobnicate"], "there is no command `frobnicate`"),
        (&["profile"], "name the root to profile"),
        (&["profile", "a", "b"], "name one root to profile"),
        (&["profile", "a", "--bogus"], "unknown option `--bogus`"),
        (&["profile", "a", "--output"], "`--output` needs a value"),
        (
            &["profile", "a", "--output", "x", "--output", "y"],
            "`--output` is given twice",
        ),
        (
            &["spec", "--id", "x", "--entries", "9"],
            "name the profile to read",
        ),
        (&["spec", "p", "--entries", "9"], "`--id` is needed"),
        (&["spec", "p", "--id", "x"], "`--entries` is needed"),
        (
            &["spec", "p", "--id", "Home", "--entries", "9"],
            "`--id` takes 1 to 64 lowercase ASCII letters",
        ),
        (
            &["spec", "p", "--id", "x", "--entries", "many"],
            "`--entries` takes a whole number, not `many`",
        ),
        (
            &["spec", "p", "--id", "x", "--entries", "0"],
            "`--entries` takes 1 to 10100000, not 0",
        ),
        (
            &["spec", "p", "--id", "x", "--entries", "10100001"],
            "`--entries` takes 1 to 10100000",
        ),
        (
            &["spec", "p", "--id", "x", "--entries", "9", "--seed", "-1"],
            "`--seed` takes a whole number",
        ),
        (
            &["spec", "p", "--id", "x", "--entries", "9", "--frob"],
            "unknown option `--frob`",
        ),
    ];
    for (args, expected) in cases {
        let UsageError(message) = parsed(args).expect_err(&format!("{args:?} is not a command"));
        assert!(
            message.contains(expected),
            "{args:?}: `{expected}` is not in `{message}`"
        );

        let (status, stdout, stderr) = run(args);
        assert_eq!(status, USAGE, "{args:?}");
        assert!(
            stdout.is_empty(),
            "a usage error writes nothing to standard output"
        );
        assert!(stderr.contains(expected) && stderr.contains("excise-shape --help"));
    }
}

#[test]
fn a_profile_goes_to_standard_output_with_a_summary_of_counts_on_standard_error() {
    let scratch = Scratch::new();
    let root = small_tree(&scratch);

    let (status, stdout, stderr) = run(&["profile", root.to_str().expect("UTF-8")]);

    assert_eq!(status, SUCCESS, "{stderr}");
    let shape = HarnessShapeProfile::from_json_str(&stdout).expect("a profile");
    shape.check().expect("a consistent profile");
    assert_eq!(shape.entries.total, 5, "two folders and three files");
    assert_eq!(shape.max_depth, 3);
    assert!(stdout.ends_with("}\n"));
    assert!(
        stderr.starts_with("excise-shape: profiled 5 entries (2 folders, 3 files, 0 symbolic links, 0 others) to a depth of 3"),
        "{stderr}"
    );
    // The summary is counts; nothing of the tree is in it.
    for name in [
        "alpha",
        "beta",
        "one.txt",
        "two.txt",
        "three.txt",
        "tree",
        "excise-shape-test",
    ] {
        assert!(
            !stderr.contains(name) && !stdout.contains(name),
            "`{name}` is in the output"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_profile_written_to_a_file_is_private_and_never_replaces_a_file() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};

    let scratch = Scratch::new();
    let root = small_tree(&scratch);
    let root = root.to_str().expect("UTF-8");
    let out = scratch.join("profiles").join("private").join("home.json");
    let out_text = out.to_str().expect("UTF-8");

    let (status, stdout, stderr) = run(&["profile", root, "--output", out_text]);

    assert_eq!(status, SUCCESS, "{stderr}");
    assert!(stdout.is_empty(), "the profile went to the file");
    let written = fs::read_to_string(&out).expect("the file");
    assert!(HarnessShapeProfile::from_json_str(&written).is_ok());
    let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
    assert_eq!(mode(&out), 0o600, "readable by its owner only");
    assert_eq!(
        mode(out.parent().expect("a parent")),
        0o700,
        "a directory made for it is private too"
    );
    assert_eq!(fs::metadata(&out).expect("metadata").nlink(), 1);

    // A second run refuses, before it walks anything, and leaves the first alone.
    let (status, _, stderr) = run(&["profile", root, "--output", out_text]);
    assert_eq!(status, FAILURE);
    assert!(
        stderr.contains("the output file already exists; a profile never replaces a file"),
        "{stderr}"
    );
    assert_eq!(fs::read_to_string(&out).expect("the file"), written);

    // A link at the path is refused too, and what it points at is left alone.
    let precious = scratch.join("precious");
    fs::write(&precious, b"keep me").expect("a file");
    let link = scratch.join("link.json");
    symlink(&precious, &link).expect("a link");
    let (status, _, stderr) = run(&["profile", root, "--output", link.to_str().expect("UTF-8")]);
    assert_eq!(status, FAILURE, "{stderr}");
    assert_eq!(fs::read(&precious).expect("a file"), b"keep me");
}

#[test]
fn a_root_that_cannot_be_profiled_is_a_failure_that_writes_nothing_and_names_no_path() {
    let scratch = Scratch::new();
    let out = scratch.join("never.json");
    let out_text = out.to_str().expect("UTF-8");
    // Roots that cannot be profiled, each at a path that no message may repeat.
    let file = scratch.join("zq9xk-a-file");
    fs::write(&file, b"x").expect("a file");
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut roots = vec![scratch.join("zq9xk-missing"), file];
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        // A folder that cannot be opened or listed, when this process is not root.
        if !super::tests::running_as_root() {
            let locked = scratch.join("zq9xk-locked");
            fs::create_dir(&locked).expect("a folder");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
            roots.push(locked);
        }
    }

    for root in &roots {
        let (status, stdout, stderr) = run(&[
            "profile",
            root.to_str().expect("UTF-8"),
            "--output",
            out_text,
        ]);

        assert_eq!(status, FAILURE);
        assert!(stdout.is_empty());
        assert!(stderr.contains("cannot open the root"), "{stderr}");
        for secret in ["zq9xk", scratch.path().to_str().expect("UTF-8")] {
            assert!(
                !stderr.contains(secret),
                "the message names the path of the root: {stderr}"
            );
        }
        assert!(!out.exists(), "no half-written profile");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let _ = fs::set_permissions(
            scratch.join("zq9xk-locked"),
            fs::Permissions::from_mode(0o700),
        );
    }
}

#[test]
fn no_error_of_the_walk_names_a_path() {
    // `WalkError` keeps no path, so none can be printed, whatever prints it.
    for error in [
        WalkError::Root {
            source: io::Error::from(io::ErrorKind::PermissionDenied),
        },
        WalkError::List {
            source: io::Error::from(io::ErrorKind::NotFound),
        },
    ] {
        let text = error.to_string();
        assert!(
            text.starts_with("cannot open the root: ")
                || text.starts_with("cannot list the root: "),
            "{text}"
        );
        assert!(!text.contains('/') && !text.contains('\\'), "{text}");
    }
}

#[test]
fn an_output_inside_the_root_is_made_after_the_walk_and_is_not_in_its_profile() {
    let scratch = Scratch::new();
    let root = small_tree(&scratch);
    let before = profile(&root, WalkOptions::default()).expect("the walk");
    let out = root.join("profiles").join("private").join("home.json");

    let (status, stdout, stderr) = run(&[
        "profile",
        root.to_str().expect("UTF-8"),
        "--output",
        out.to_str().expect("UTF-8"),
    ]);

    assert_eq!(status, SUCCESS, "{stderr}");
    assert!(stdout.is_empty(), "the profile went to the file");
    let written = HarnessShapeProfile::from_json_str(&fs::read_to_string(&out).expect("the file"))
        .expect("a profile");
    assert_eq!(
        written.entries, before.entries,
        "a profile never counts its own output, which is made after the walk"
    );
    assert_eq!(written.max_depth, before.max_depth);
    // The output is in the tree now, with the two folders made for it, and a walk after it counts
    // all three.
    let after = profile(&root, WalkOptions::default()).expect("the walk");
    assert_eq!(after.entries.total, before.entries.total + 3);
    assert_eq!(after.entries.directories, before.entries.directories + 2);
    assert_eq!(after.entries.files, before.entries.files + 1);
}

/// Folders that exist are taken as they are, a link among them followed as for any path a person
/// types; a folder that is missing is made on its own, so that anything that appears at its name
/// before it is made, a link included, is refused and nothing is written through it.
#[cfg(unix)]
#[test]
fn a_missing_folder_that_becomes_a_link_before_it_is_made_is_refused() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let base = scratch.join("base");
    fs::create_dir(&base).expect("a folder");

    // A link among the folders that exist is followed.
    let real = scratch.join("real");
    fs::create_dir(&real).expect("a folder");
    symlink(&real, base.join("via")).expect("a link");
    cli::write_new_file(&base.join("via").join("out.json"), "through").expect("written");
    assert_eq!(
        fs::read_to_string(real.join("out.json")).expect("the file"),
        "through"
    );

    // The first folder that is missing, or a later one, becomes a link just before it is made.
    for (index, (label, replaced)) in [
        ("the first missing folder", base.join("one")),
        ("a later missing folder", base.join("two").join("inner")),
    ]
    .into_iter()
    .enumerate()
    {
        let elsewhere = scratch.join(&format!("elsewhere-{index}"));
        fs::create_dir(&elsewhere).expect("a folder");
        let target = replaced.join("deeper").join("profile.json");
        let mut seen = Vec::new();

        let error = cli::create_new_file(&target, "text", &mut |folder: &Path| {
            seen.push(folder.to_path_buf());
            if folder == replaced {
                symlink(&elsewhere, folder).expect("a link appears");
            }
        })
        .expect_err(label);

        assert_eq!(
            error.kind(),
            io::ErrorKind::AlreadyExists,
            "{label}: {error}"
        );
        assert!(
            seen.contains(&replaced),
            "{label}: the folder was about to be made"
        );
        assert_eq!(
            fs::read_dir(&elsewhere).expect("a folder").count(),
            0,
            "{label}: nothing was written through the link"
        );
        assert!(!target.exists());
    }

    // A link at the name of the file is refused too, and what it points at is left alone.
    let precious = scratch.join("precious");
    fs::write(&precious, b"keep me").expect("a file");
    symlink(&precious, base.join("link.json")).expect("a link");
    let error = cli::write_new_file(&base.join("link.json"), "text").expect_err("a link");
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert_eq!(fs::read(&precious).expect("a file"), b"keep me");
}

#[test]
fn a_spec_is_built_from_a_profile_file_and_written_as_toml() {
    let scratch = Scratch::new();
    let root = small_tree(&scratch);
    let (status, profile_text, _) = run(&["profile", root.to_str().expect("UTF-8")]);
    assert_eq!(status, SUCCESS);
    let profile_path = scratch.join("profile.json");
    fs::write(&profile_path, &profile_text).expect("the profile");
    let profile_path = profile_path.to_str().expect("UTF-8");

    let (status, stdout, stderr) = run(&[
        "spec",
        profile_path,
        "--id",
        "tiny-shape",
        "--entries",
        "40",
        "--seed",
        "5",
        "--max-file-bytes",
        "64",
    ]);

    assert_eq!(status, SUCCESS, "{stderr}");
    assert!(
        stderr.starts_with("excise-shape: `tiny-shape` plans 40 entries in 3 levels, shaped like a profile of 5 entries"),
        "{stderr}"
    );
    let spec = FixtureSpec::from_toml_str(&stdout).expect("a spec file");
    assert_eq!(spec.id, "tiny-shape");
    assert_eq!(spec.seed, 5);
    assert_eq!(spec.planned_entry_count(), 40);
    assert!(stdout.contains("max_file_bytes = 64"));

    // The same command line gives the same text.
    let (_, again, _) = run(&[
        "spec",
        profile_path,
        "--id",
        "tiny-shape",
        "--entries",
        "40",
        "--seed",
        "5",
        "--max-file-bytes",
        "64",
    ]);
    assert_eq!(stdout, again);

    // To a file: private, and never over another.
    let out = scratch.join("specs").join("tiny-shape.toml");
    let out_text = out.to_str().expect("UTF-8");
    let (status, stdout, stderr) = run(&[
        "spec",
        profile_path,
        "--id",
        "tiny-shape",
        "--entries",
        "40",
        "--output",
        out_text,
    ]);
    assert_eq!(status, SUCCESS, "{stderr}");
    assert!(stdout.is_empty());
    assert!(FixtureSpec::load(out.parent().expect("a parent"), "tiny-shape").is_ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            fs::metadata(&out).expect("metadata").permissions().mode() & 0o777,
            0o600
        );
    }
    let (status, _, stderr) = run(&[
        "spec",
        profile_path,
        "--id",
        "tiny-shape",
        "--entries",
        "40",
        "--output",
        out_text,
    ]);
    assert_eq!(status, FAILURE);
    assert!(stderr.contains("exists"), "{stderr}");
}

#[test]
fn a_spec_is_refused_for_a_profile_that_is_not_one_and_a_size_that_cannot_be_met() {
    let scratch = Scratch::new();
    let root = small_tree(&scratch);
    let (_, profile_text, _) = run(&["profile", root.to_str().expect("UTF-8")]);
    let write = |name: &str, text: &str| {
        let path = scratch.join(name);
        fs::write(&path, text).expect("a file");
        path.to_str().expect("UTF-8").to_owned()
    };
    let ask = |path: &str, entries: &str| run(&["spec", path, "--id", "x", "--entries", entries]);

    // Not a profile: other JSON, a field that does not belong, a document of another version,
    // numbers that disagree, and text that is not JSON.
    let other_kind = profile_text.replace("harness-shape-profile", "harness-counts");
    let future = profile_text.replace("\"schema_version\": 1", "\"schema_version\": 2");
    let unknown = profile_text.replace("\"platform\"", "\"owner\": \"tom\",\n  \"platform\"");
    let lying = profile_text.replacen("\"files\": 3", "\"files\": 4", 1);
    for (name, text) in [
        ("other.json", other_kind.as_str()),
        ("future.json", future.as_str()),
        ("unknown.json", unknown.as_str()),
        ("lying.json", lying.as_str()),
        ("text.json", "this is not json"),
        ("empty.json", ""),
    ] {
        let (status, stdout, stderr) = ask(&write(name, text), "100");
        assert_eq!(status, FAILURE, "{name}: {stderr}");
        assert!(
            stdout.is_empty(),
            "{name}: nothing is written for a profile that is refused"
        );
        assert!(stderr.contains("excise-shape: "), "{name}: {stderr}");
    }
    let (_, _, stderr) = ask(&write("future2.json", &future), "100");
    assert!(stderr.contains("unsupported schema_version 2"), "{stderr}");
    let (_, _, stderr) = ask(&write("lying2.json", &lying), "100");
    assert!(stderr.contains("not consistent"), "{stderr}");

    // A profile no real tree makes is refused before it is read in full.
    let (status, _, stderr) = ask(
        &write(
            "huge.json",
            &" ".repeat(usize::try_from(cli::MAX_PROFILE_BYTES).expect("fits") + 1),
        ),
        "100",
    );
    assert_eq!(status, FAILURE);
    assert!(stderr.contains("larger than"), "{stderr}");

    // A file that is not there.
    let (status, _, stderr) = ask(scratch.join("absent.json").to_str().expect("UTF-8"), "100");
    assert_eq!(status, FAILURE);
    assert!(
        stderr.contains("No such file") || stderr.contains("cannot find"),
        "{stderr}"
    );

    // A size the tree cannot be kept deep in.
    let good = write("good.json", &profile_text);
    let (status, _, stderr) = ask(&good, "3");
    assert_eq!(status, FAILURE);
    assert!(
        stderr.contains("needs at least 4 entries, not 3"),
        "{stderr}"
    );
    // And a size that is not a spec's to plan.
    let (status, _, _) = ask(&good, "10100001");
    assert_eq!(status, USAGE);
}

/// The output of `profile` can be inside the root, so what is said about an output that exists
/// names no path: no message of the command names the root or anything below it. (A spec's own
/// message may name its file: nothing in a spec's run comes from a profiled tree.)
#[test]
fn an_output_that_exists_inside_the_root_is_refused_without_naming_a_path() {
    let scratch = Scratch::new();
    let root = small_tree(&scratch);
    let existing = root.join("zq9xk-existing.json");
    fs::write(&existing, b"keep me").expect("a file");

    let (status, stdout, stderr) = run(&[
        "profile",
        root.to_str().expect("UTF-8"),
        "--output",
        existing.to_str().expect("UTF-8"),
    ]);

    assert_eq!(status, FAILURE, "{stderr}");
    assert!(stdout.is_empty());
    for secret in [
        "zq9xk",
        root.to_str().expect("UTF-8"),
        scratch.path().to_str().expect("UTF-8"),
    ] {
        assert!(
            !stderr.contains(secret),
            "the message names a path below the root: {stderr}"
        );
    }
    assert!(stderr.contains("already exists"), "{stderr}");
    assert_eq!(fs::read(&existing).expect("a file"), b"keep me");
}

/// A spec keeps at most 32 levels, so the fewest entries `spec` takes is one more than the levels
/// the profile keeps, and for a profile deeper than that it is 33, not one more than its depth.
#[test]
fn the_fewest_entries_a_deeper_profile_needs_are_33_and_the_help_says_so() {
    let scratch = Scratch::new();
    // A chain of 40 folders, each holding a file: a tree 41 levels deep.
    let root = scratch.join("deep");
    let mut folder = root.clone();
    for _ in 0..40 {
        folder.push("d");
        fs::create_dir_all(&folder).expect("a folder");
        fs::write(folder.join("f"), b"x").expect("a file");
    }
    let (status, profile_text, _) = run(&["profile", root.to_str().expect("UTF-8")]);
    assert_eq!(status, SUCCESS);
    let profile_path = scratch.join("deep.json");
    fs::write(&profile_path, &profile_text).expect("a file");
    let ask = |entries: &str| {
        run(&[
            "spec",
            profile_path.to_str().expect("UTF-8"),
            "--id",
            "x",
            "--entries",
            entries,
        ])
    };

    let (status, _, stderr) = ask("33");
    assert_eq!(
        status, SUCCESS,
        "a tree of 41 levels is kept 32 deep: {stderr}"
    );
    let (status, _, stderr) = ask("32");
    assert_eq!(status, FAILURE);
    assert!(
        stderr.contains("needs at least 33 entries, not 32"),
        "{stderr}"
    );

    let entries_help: Vec<&str> = cli::SPEC_HELP
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("--entries N"))
        .take_while(|line| !line.trim_start().starts_with("--seed S"))
        .collect();
    let entries_help = entries_help.join(" ");
    assert!(
        entries_help.contains("32"),
        "the help does not say that a deep profile keeps 32 levels: {entries_help}"
    );
}
