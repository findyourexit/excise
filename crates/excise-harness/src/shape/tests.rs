//! The shape tools: the walk against the oracle, what a profile never holds, the safety of the
//! walk, and the round trip from a tree to a profile to a spec to a tree.

use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use serde_json::Value;

use super::{
    DeriveError, HANDLE_BUDGET, SpecRequest, WalkError, WalkOptions,
    dir::Name,
    profile, spec_from_profile,
    walk::{Seam, profile_under},
};
#[cfg(unix)]
use super::{SHAPED_ROOT, render_spec};
#[cfg(unix)]
use crate::fixture::{Plan, names};
use crate::{
    fixture::{
        FixtureCache, FixtureSpec, MaterializeOptions, Materialized, NodeKind, Oracle, OracleEntry,
        remove_tree,
    },
    report::{Document, HarnessShapeProfile, ShapeDepth, ShapeHardLinks, ShapeHistogram},
};

// ---------------------------------------------------------------------------------------------
// Scratch trees.

/// A private temporary directory that is removed when it drops, even when it holds directories
/// that cannot be listed.
pub(super) struct Scratch {
    dir: Option<tempfile::TempDir>,
}

impl Scratch {
    pub(super) fn new() -> Self {
        let dir = tempfile::Builder::new()
            .prefix("excise-shape-test-")
            .tempdir()
            .unwrap_or_else(|error| panic!("cannot create a temporary directory: {error}"));
        Self { dir: Some(dir) }
    }

    pub(super) fn path(&self) -> &Path {
        self.dir
            .as_ref()
            .map_or_else(|| Path::new(""), tempfile::TempDir::path)
    }

    pub(super) fn join(&self, name: &str) -> PathBuf {
        self.path().join(name)
    }

    fn cache(&self) -> FixtureCache {
        FixtureCache::at(self.join("cache"))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(dir) = self.dir.take() {
            let path = dir.path().to_path_buf();
            let _ = remove_tree(&path);
            drop(dir);
        }
    }
}

fn master_of(scratch: &Scratch, spec: &FixtureSpec) -> Materialized {
    scratch
        .cache()
        .materialize(spec, &MaterializeOptions::default())
        .unwrap_or_else(|error| panic!("{}: {error}", spec.id))
}

fn bundled_master(scratch: &Scratch, id: &str) -> Materialized {
    let spec = FixtureSpec::load_bundled(id).unwrap_or_else(|error| panic!("{id}: {error}"));
    master_of(scratch, &spec)
}

fn walk(root: &Path) -> HarnessShapeProfile {
    profile(root, WalkOptions::default())
        .unwrap_or_else(|error| panic!("the walk of {}: {error}", root.display()))
}

// ---------------------------------------------------------------------------------------------
// What the oracle counts, worked out by plain code that shares nothing with the walk.

/// The class of powers of two that holds `value`, from its leading zeros.
fn class(value: u64) -> u64 {
    if value == 0 { 0 } else { 1 << value.ilog2() }
}

fn histogram_of(values: impl IntoIterator<Item = u64>, key: impl Fn(u64) -> u64) -> ShapeHistogram {
    let mut histogram = ShapeHistogram::default();
    for value in values {
        histogram.count += 1;
        histogram.total += value;
        histogram.max = histogram.max.max(value);
        histogram.buckets.add(key(value), 1);
    }
    histogram
}

/// The profile the oracle's facts add up to: every number worked out from the entries of the
/// oracle's own walk.
fn expected_from(oracle: &Oracle) -> HarnessShapeProfile {
    let entries: Vec<&OracleEntry> = oracle.descendants().collect();

    // Per depth.
    let mut depths: Vec<ShapeDepth> = Vec::new();
    for entry in &entries {
        let depth = entry.path.depth();
        while depths.len() < depth {
            depths.push(ShapeDepth {
                depth: u32::try_from(depths.len() + 1).expect("a small depth"),
                directories: 0,
                files: 0,
                symlinks: 0,
                others: 0,
                bytes: 0,
            });
        }
        let level = &mut depths[depth - 1];
        match entry.kind {
            NodeKind::Directory => level.directories += 1,
            NodeKind::File => {
                level.files += 1;
                level.bytes += entry.size;
            }
            NodeKind::Symlink => level.symlinks += 1,
            NodeKind::Other => level.others += 1,
        }
    }
    let sum = |pick: fn(&ShapeDepth) -> u64| depths.iter().map(pick).sum::<u64>();
    let mut expected = walk_template();
    expected.entries.directories = sum(|level| level.directories);
    expected.entries.files = sum(|level| level.files);
    expected.entries.symlinks = sum(|level| level.symlinks);
    expected.entries.others = sum(|level| level.others);
    expected.entries.total = entries.len() as u64;
    expected.max_depth = u32::try_from(depths.len()).expect("a small depth");
    expected.depths = depths;

    // What each folder that was listed holds: (everything, folders, files).
    let mut listed: BTreeMap<Vec<u8>, (u64, u64, u64)> = BTreeMap::new();
    listed.insert(Vec::new(), (0, 0, 0));
    for entry in &entries {
        if entry.kind == NodeKind::Directory && entry.readable {
            listed.insert(entry.path.as_bytes().to_vec(), (0, 0, 0));
        }
    }
    for entry in &entries {
        let held = listed
            .get_mut(entry.path.parent_bytes())
            .expect("the parent of an entry was listed");
        held.0 += 1;
        match entry.kind {
            NodeKind::Directory => held.1 += 1,
            NodeKind::File => held.2 += 1,
            NodeKind::Symlink | NodeKind::Other => {}
        }
    }
    expected.children_per_directory = histogram_of(listed.values().map(|held| held.0), class);
    expected.subdirectories_per_directory = histogram_of(listed.values().map(|held| held.1), class);
    expected.files_per_directory = histogram_of(listed.values().map(|held| held.2), class);

    let of_kind = |kind: NodeKind| entries.iter().filter(move |entry| entry.kind == kind);
    expected.file_sizes = histogram_of(of_kind(NodeKind::File).map(|entry| entry.size), class);
    let length = |entry: &OracleEntry| entry.path.file_name().expect("a name").len() as u64;
    let lengths = |kind: NodeKind| histogram_of(of_kind(kind).map(|e| length(e)), |value| value);
    expected.name_lengths.directories = lengths(NodeKind::Directory);
    expected.name_lengths.files = lengths(NodeKind::File);
    expected.name_lengths.symlinks = lengths(NodeKind::Symlink);

    expected.hard_links = expected_hard_links(oracle);

    // Symbolic links: how many there are, which is all a profile says of them.
    expected.symbolic_links.count = expected.entries.symlinks;

    expected.unreadable.directories = entries
        .iter()
        .filter(|entry| entry.kind == NodeKind::Directory && !entry.readable)
        .count() as u64;
    expected.platform.identity = oracle.platform.identity;
    expected
}

/// The hard links the oracle's facts add up to: it groups every entry with more than one name by
/// device and inode, and the entry of the first name of a group says the size of its file, which
/// every name of the file has.
fn expected_hard_links(oracle: &Oracle) -> ShapeHardLinks {
    // For each file with several names: the names found, the link count, and the size.
    let linked_files: Vec<(u64, u64, u64)> = oracle
        .hard_links
        .iter()
        .filter_map(|group| {
            let first = oracle.find(&group.paths[0])?;
            (first.kind == NodeKind::File).then_some((
                group.paths.len() as u64,
                group.nlink,
                first.size,
            ))
        })
        .collect();

    let mut by_file_size: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
    for (names, _, size) in &linked_files {
        by_file_size.entry(class(*size)).or_default().push(*names);
    }
    ShapeHardLinks {
        groups: linked_files.len() as u64,
        names: linked_files.iter().map(|(names, _, _)| names).sum(),
        incomplete_groups: linked_files
            .iter()
            .filter(|(names, links, _)| links > names)
            .count() as u64,
        group_sizes: histogram_of(linked_files.iter().map(|(names, _, _)| *names), class),
        group_sizes_by_file_size: by_file_size
            .into_iter()
            .map(|(file_class, sizes)| (file_class, histogram_of(sizes, class)))
            .collect(),
    }
}

/// A profile with nothing in it, of this platform.
fn walk_template() -> HarnessShapeProfile {
    let empty = Scratch::new();
    walk(empty.path())
}

#[cfg(unix)]
pub(super) fn running_as_root() -> bool {
    let dir = Scratch::new();
    let locked = dir.join("locked");
    fs::create_dir(&locked).expect("a directory");
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod");
    }
    let readable = fs::read_dir(&locked).is_ok();
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o700));
    }
    readable
}

/// The fixtures the walk is held to the oracle on: every class the harness generates that a walk
/// by `std` can reach, a chain deeper than `PATH_MAX` included.
const ORACLE_FIXTURES: &[&str] = &[
    "node-modules-2k",
    "wide-1k",
    "deep-past-path-max",
    "identity-small",
    "hostile-small",
    "all-classes-small",
    "delete-folder",
    "mount-boundary",
];

#[test]
fn the_profile_of_a_generated_fixture_is_what_the_oracle_counts_for_the_same_tree() {
    let scratch = Scratch::new();
    for id in ORACLE_FIXTURES {
        let master = bundled_master(&scratch, id);
        let oracle = Oracle::collect(&master.root).expect("the oracle walk");

        let actual = walk(&master.root);
        let expected = expected_from(&oracle);

        assert_eq!(actual.entries, expected.entries, "{id}: entries by kind");
        assert_eq!(
            actual.max_depth, expected.max_depth,
            "{id}: the deepest level"
        );
        assert_eq!(
            actual.depths, expected.depths,
            "{id}: entries and bytes per depth"
        );
        assert_eq!(
            actual.children_per_directory, expected.children_per_directory,
            "{id}: children per directory"
        );
        assert_eq!(
            actual.subdirectories_per_directory, expected.subdirectories_per_directory,
            "{id}: folders per directory"
        );
        assert_eq!(
            actual.files_per_directory, expected.files_per_directory,
            "{id}: files per directory"
        );
        assert_eq!(actual.file_sizes, expected.file_sizes, "{id}: file sizes");
        assert_eq!(
            actual.name_lengths, expected.name_lengths,
            "{id}: name lengths"
        );
        assert_eq!(actual.hard_links, expected.hard_links, "{id}: hard links");
        assert_eq!(
            actual.symbolic_links, expected.symbolic_links,
            "{id}: symbolic links"
        );
        assert_eq!(
            actual.unreadable, expected.unreadable,
            "{id}: unreadable entries"
        );
        assert_eq!(actual.platform, expected.platform, "{id}: platform");
        assert_eq!(actual.walk.mount_points_skipped, 0, "{id}: nothing to skip");
        actual
            .check()
            .unwrap_or_else(|problems| panic!("{id}: {problems}"));
    }
}

#[test]
fn the_classes_of_identity_and_hostile_fixtures_show_up_in_the_profile() {
    let scratch = Scratch::new();

    #[cfg(unix)]
    {
        let identity = walk(&bundled_master(&scratch, "identity-small").root);
        assert!(identity.hard_links.groups > 0, "identity-small links files");
        assert_eq!(identity.hard_links.incomplete_groups, 0);
        assert!(identity.symbolic_links.count > 0);

        if !running_as_root() {
            let hostile = walk(&bundled_master(&scratch, "hostile-small").root);
            assert_eq!(
                hostile.unreadable.directories, 2,
                "the directories of mode 000 and 100"
            );
            assert_eq!(hostile.unreadable.errors, 0);
        }
    }
    let wide = walk(&bundled_master(&scratch, "wide-1k").root);
    assert_eq!(wide.depths.len(), 2, "the wide directory and its files");
    assert_eq!(wide.files_per_directory.max, 1_000);
}

// ---------------------------------------------------------------------------------------------
// What a profile never holds.

/// Every string a JSON value holds as a value, and every key it holds as a key.
#[cfg(unix)]
fn strings_and_keys(value: &Value, strings: &mut Vec<String>, keys: &mut Vec<String>) {
    match value {
        Value::String(text) => strings.push(text.clone()),
        Value::Array(items) => {
            for item in items {
                strings_and_keys(item, strings, keys);
            }
        }
        Value::Object(members) => {
            for (key, member) in members {
                keys.push(key.clone());
                strings_and_keys(member, strings, keys);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// The names of a scratch tree that nothing else would ever say, and the text a link points at.
#[cfg(unix)]
const DISTINCTIVE: &str = "zq9xk";

/// A scratch tree with distinctive names, links whose targets are distinctive, and a link to a
/// directory outside the tree. Returns the root.
#[cfg(unix)]
fn distinctive_tree(scratch: &Scratch) -> PathBuf {
    use std::os::unix::fs::symlink;

    let outside = scratch.join("zq9xk-outside");
    fs::create_dir(&outside).expect("outside");
    for index in 0..5 {
        fs::write(outside.join(format!("zq9xk-outside-file-{index}")), b"x").expect("a file");
    }
    let root = scratch.join("zq9xk-root");
    fs::create_dir(&root).expect("root");
    fs::create_dir(root.join("zq9xk-secret-ledger")).expect("a directory");
    fs::write(
        root.join("zq9xk-secret-ledger").join("zq9xk-passwords.txt"),
        b"hunter2",
    )
    .expect("a file");
    fs::write(root.join("zq9xk-diary.txt"), b"dear diary").expect("a file");
    symlink(
        "../zq9xk-private/zq9xk-target",
        root.join("zq9xk-dangling-link"),
    )
    .expect("a link");
    symlink(&outside, root.join("zq9xk-escape-link")).expect("a link");
    symlink("zq9xk-diary.txt", root.join("zq9xk-valid-link")).expect("a link");
    fs::hard_link(
        root.join("zq9xk-diary.txt"),
        root.join("zq9xk-secret-ledger").join("zq9xk-second-name"),
    )
    .expect("a hard link");
    root
}

#[cfg(unix)]
#[test]
fn a_profile_holds_no_name_path_target_or_owner() {
    let scratch = Scratch::new();
    let root = distinctive_tree(&scratch);
    let hostile = bundled_master(&scratch, "hostile-small");
    let identity = bundled_master(&scratch, "identity-small");

    for tree in [&root, &hostile.root, &identity.root] {
        let shape = walk(tree);
        let text = shape.to_json_pretty().expect("a document");
        let document: Value = serde_json::from_str(&text).expect("JSON");

        // No name anywhere in the text: the distinctive ones, the root's path and its random
        // directory name, the marker's name, and every hostile name in the catalog, as the raw
        // bytes, as JSON writes it, and in pieces.
        assert!(
            !text.contains(DISTINCTIVE),
            "a distinctive name is in the profile"
        );
        assert!(
            !text.contains(&tree.display().to_string()),
            "the root's path is in the profile"
        );
        for part in scratch.path().components() {
            let part = part.as_os_str().to_string_lossy();
            if part.starts_with("excise-shape-test-") {
                assert!(
                    !text.contains(part.as_ref()),
                    "the scratch name is in the profile"
                );
            }
        }
        assert!(
            !text.contains("excise-harness-owned"),
            "the marker's name is in the profile"
        );
        let catalog: Vec<Vec<u8>> = [
            names::control_character_names(),
            names::bidi_names(),
            names::escape_sequence_names(),
            names::newline_names(),
            names::invalid_utf8_names(),
            names::maximum_length_names(),
        ]
        .concat();
        for name in &catalog {
            let lossy = String::from_utf8_lossy(name);
            let escaped = serde_json::to_string(lossy.as_ref()).expect("a string");
            let escaped = escaped.trim_matches('"');
            assert!(
                !text
                    .as_bytes()
                    .windows(name.len())
                    .any(|window| window == name.as_slice()),
                "the bytes of {name:?} are in the profile"
            );
            assert!(!text.contains(escaped), "{escaped} is in the profile");
        }
        for fragment in [
            "ctl-",
            "bidi",
            "esc-",
            "nl-",
            "pwned",
            "fdp",
            "rlo",
            "line1",
            "locked",
            "stuck",
            "inner",
            "deep.txt",
            "write-only",
            "missing",
            "again",
            "pair-",
            "node_modules",
            "keep",
        ] {
            assert!(!text.contains(fragment), "`{fragment}` is in the profile");
        }

        // And no string at all but the two the document is made of: what it says it is, and the
        // operating system. Every key is a field of the schema or a number.
        let (mut strings, mut keys) = (Vec::new(), Vec::new());
        strings_and_keys(&document, &mut strings, &mut keys);
        strings.sort();
        let mut fixed = vec![
            "harness-shape-profile".to_owned(),
            std::env::consts::OS.to_owned(),
        ];
        fixed.sort();
        assert_eq!(strings, fixed, "the only strings are the kind and the OS");
        let schema: Value = serde_json::from_str(HarnessShapeProfile::SCHEMA_JSON).expect("schema");
        let mut vocabulary = Vec::new();
        collect_property_names(&schema, &mut vocabulary);
        for key in keys {
            assert!(
                vocabulary.contains(&key) || key.bytes().all(|byte| byte.is_ascii_digit()),
                "the key `{key}` is neither a field of the schema nor a bucket number"
            );
        }
    }
}

/// Every property name a schema declares, wherever it declares one.
#[cfg(unix)]
fn collect_property_names(schema: &Value, out: &mut Vec<String>) {
    match schema {
        Value::Object(members) => {
            if let Some(Value::Object(properties)) = members.get("properties") {
                out.extend(properties.keys().cloned());
            }
            for member in members.values() {
                collect_property_names(member, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_property_names(item, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// The safety of the walk.

#[cfg(unix)]
#[test]
fn the_walk_never_follows_a_link_out_of_the_root() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = distinctive_tree(&scratch);
    // A tree outside the root that a followed link would reach: 5 files, and below them more.
    let outside = scratch.join("zq9xk-outside");
    fs::create_dir(outside.join("nested")).expect("nested");
    fs::write(outside.join("nested").join("deep-file"), b"x").expect("a file");
    // A link through a link, a link to a file outside, and a link to the root's own parent.
    symlink(&outside, root.join("zq9xk-escape-again")).expect("a link");
    symlink(
        outside.join("zq9xk-outside-file-0"),
        root.join("zq9xk-file-escape"),
    )
    .expect("a link");
    symlink("..", root.join("zq9xk-up")).expect("a link");

    let shape = walk(&root);

    // The root holds: a folder, a file, and six links; the folder holds a file and a name of the
    // file at the root. Nothing of what the links point at is counted.
    assert_eq!(shape.entries.directories, 1);
    assert_eq!(shape.entries.files, 3);
    assert_eq!(shape.entries.symlinks, 6);
    assert_eq!(shape.entries.others, 0);
    assert_eq!(shape.entries.total, 10);
    assert_eq!(shape.max_depth, 2);
    // Six links, to a folder and files outside the root, to a link, to `..`, and to a file that is
    // not there. Each is one link, and nothing about where it leads is asked.
    assert_eq!(shape.symbolic_links.count, 6);
    // The diary and its second name are one file with two names, both inside the root.
    assert_eq!(shape.hard_links.groups, 1);
    assert_eq!(shape.hard_links.names, 2);
    assert_eq!(shape.hard_links.incomplete_groups, 0);
    assert_eq!(shape.unreadable.errors, 0);
    shape.check().expect("a consistent profile");
}

#[cfg(unix)]
#[test]
fn a_loop_of_links_is_counted_as_links_and_is_never_entered() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir(&root).expect("root");
    symlink("self", root.join("self")).expect("a link");
    symlink("pair-b", root.join("pair-a")).expect("a link");
    symlink("pair-a", root.join("pair-b")).expect("a link");
    fs::create_dir(root.join("cycle")).expect("a directory");
    symlink("../cycle", root.join("cycle").join("again")).expect("a link");

    let shape = walk(&root);

    assert_eq!(shape.entries.symlinks, 4);
    assert_eq!(shape.symbolic_links.count, 4);
    assert_eq!(shape.unreadable.errors, 0, "a loop of links is no error");
    assert_eq!(
        shape.entries.directories, 1,
        "the link to `../cycle` was not entered"
    );
}

#[test]
fn the_root_must_be_a_directory_and_not_a_link_to_one() {
    let scratch = Scratch::new();
    let file = scratch.join("file");
    fs::write(&file, b"x").expect("a file");
    assert!(matches!(
        profile(&file, WalkOptions::default()),
        Err(WalkError::Root { .. })
    ));
    assert!(matches!(
        profile(&scratch.join("missing"), WalkOptions::default()),
        Err(WalkError::Root { .. })
    ));
    #[cfg(unix)]
    {
        let real = scratch.join("real");
        fs::create_dir(&real).expect("a directory");
        let link = scratch.join("link");
        std::os::unix::fs::symlink(&real, &link).expect("a link");
        assert!(
            matches!(
                profile(&link, WalkOptions::default()),
                Err(WalkError::Root { .. })
            ),
            "a link to a directory is not the directory"
        );
    }
}

/// A separator after a link does not turn it into a folder. The last component of the root must
/// be a folder itself, and `link/` names the same entry as `link`, so a path that ends in a
/// separator, or in `.`, is held to the same rule as one that does not: the `/` a shell adds when
/// it completes the name of a link must not make the walk follow it.
#[cfg(unix)]
#[test]
fn a_root_that_is_a_link_is_refused_whatever_follows_its_name() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let real = scratch.join("real");
    fs::create_dir_all(real.join("inside")).expect("folders");
    fs::write(real.join("file"), b"x").expect("a file");
    symlink(&real, scratch.join("link")).expect("a link");

    for typed in ["link", "link/", "link//", "link/."] {
        let root = scratch.path().join(typed);
        assert!(
            matches!(
                profile(&root, WalkOptions::default()),
                Err(WalkError::Root { .. })
            ),
            "`{typed}` is a link to a folder, and not a folder"
        );
    }
    // The folder itself, written the same ways, and a folder reached through a link among the
    // components, which is followed as in every path a person types.
    for typed in [
        "real",
        "real/",
        "real//",
        "real/.",
        "link/inside",
        "link/inside/",
    ] {
        let root = scratch.path().join(typed);
        if let Err(error) = profile(&root, WalkOptions::default()) {
            panic!("`{typed}` is a folder: {error}");
        }
    }
}

/// The same on Windows, where a junction is a link to a folder that needs no privilege to make:
/// opened as a reparse point it is refused, and a trailing separator must not open what it points
/// at instead.
#[cfg(windows)]
#[test]
fn a_junction_that_is_the_root_is_refused_with_a_trailing_separator_too() {
    use std::process::Command;

    let scratch = Scratch::new();
    fs::create_dir(scratch.join("real")).expect("a folder");
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(scratch.join("link"))
        .arg(scratch.join("real"))
        .output()
        .expect("cmd runs");
    assert!(made.status.success(), "a junction is made: {made:?}");

    for typed in ["link", "link\\", "link\\\\", "link/", "link\\."] {
        let root = scratch.path().join(typed);
        assert!(
            matches!(
                profile(&root, WalkOptions::default()),
                Err(WalkError::Root { .. })
            ),
            "`{typed}` is a junction, and not a folder"
        );
    }
    for typed in ["real", "real\\", "real/"] {
        let root = scratch.path().join(typed);
        if let Err(error) = profile(&root, WalkOptions::default()) {
            panic!("`{typed}` is a folder: {error}");
        }
    }
}

/// A verbatim path (`\\?\C:\...`) keeps each `.` in it as a component of its own, one that ends
/// the path too, and `Path::file_name` of a path that ends in one is `None`. The walk then took
/// the junction before that `.` for a folder on the way to it, which the system follows: it
/// opened what the junction points at. A junction written that way is refused, with the `.`
/// repeated and with a separator after it, and a folder written that way is walked.
///
/// Windows cannot run on the machine this was written on: the test is first exercised by CI's
/// Windows job.
#[cfg(windows)]
#[test]
fn a_junction_that_is_the_root_is_refused_in_the_verbatim_form_with_a_dot_after_it() {
    use std::process::Command;

    let scratch = Scratch::new();
    fs::create_dir(scratch.join("real")).expect("a folder");
    fs::write(scratch.join("real").join("file"), b"x").expect("a file");
    let made = Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(scratch.join("link"))
        .arg(scratch.join("real"))
        .output()
        .expect("cmd runs");
    assert!(made.status.success(), "a junction is made: {made:?}");

    // The scratch folder in its verbatim form, and each name with what follows it joined by hand:
    // `Path::join` onto a verbatim path would drop the `.`.
    let verbatim = fs::canonicalize(scratch.path()).expect("the scratch folder resolves");
    let written = |name: &str, after: &str| {
        let mut text = verbatim.join(name).into_os_string();
        text.push(after);
        PathBuf::from(text)
    };
    for after in [r"\", r"\.", r"\.\", r"\.\.", r"\.\.\"] {
        let root = written("link", after);
        assert!(
            matches!(
                profile(&root, WalkOptions::default()),
                Err(WalkError::Root { .. })
            ),
            "`{}` is a junction, and not a folder",
            root.display()
        );
    }
    for after in ["", r"\", r"\.", r"\.\", r"\.\.", r"\.\.\"] {
        let root = written("real", after);
        let shape = profile(&root, WalkOptions::default())
            .unwrap_or_else(|error| panic!("`{}` is a folder: {error}", root.display()));
        assert_eq!(
            shape.entries.files,
            1,
            "`{}` is the folder `real`",
            root.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn a_folder_that_cannot_be_listed_is_counted_and_the_walk_goes_on() {
    use std::os::unix::fs::PermissionsExt as _;

    if running_as_root() {
        eprintln!("skipped: a process that is root can list every folder");
        return;
    }
    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir_all(root.join("locked").join("inner")).expect("directories");
    fs::write(root.join("locked").join("file"), b"x").expect("a file");
    fs::create_dir(root.join("open")).expect("a directory");
    fs::write(root.join("open").join("file"), b"x").expect("a file");
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o000)).expect("chmod");

    let shape = walk(&root);
    fs::set_permissions(root.join("locked"), fs::Permissions::from_mode(0o700)).expect("chmod");

    assert_eq!(shape.unreadable.directories, 1);
    assert_eq!(shape.unreadable.errors, 0);
    assert_eq!(
        shape.entries.directories, 2,
        "the locked folder is a folder"
    );
    assert_eq!(shape.entries.files, 1, "what is inside it is out of reach");
    assert_eq!(
        shape.children_per_directory.count, 2,
        "the root and `open` were listed; `locked` was not"
    );
    shape.check().expect("a consistent profile");
}

#[cfg(unix)]
#[test]
fn the_walk_crosses_a_mount_point_only_when_asked() {
    use crate::fixture::{Fixtures, PrivilegedOptIn};

    let Some(opt_in) = PrivilegedOptIn::from_env() else {
        eprintln!("skipped: EXCISE_HARNESS_PRIVILEGED=1 not set");
        return;
    };
    let scratch = Scratch::new();
    let fixtures = Fixtures::new(FixtureSpec::bundled_dir(), scratch.cache());
    fs::create_dir(scratch.join("runs")).expect("a directory");
    let mut copy = fixtures
        .run_copy("mount-boundary", &scratch.join("runs"))
        .expect("a run copy");
    assert_eq!(copy.attach_volumes(opt_in).expect("a volume"), 1);

    let staying = walk(copy.root());
    let crossing = profile(
        copy.root(),
        WalkOptions {
            cross_filesystems: true,
        },
    )
    .expect("the walk");

    assert_eq!(
        staying.walk.mount_points_skipped, 1,
        "the volume's mount point"
    );
    assert!(!staying.walk.cross_filesystems && crossing.walk.cross_filesystems);
    assert_eq!(crossing.walk.mount_points_skipped, 0);
    // The volume holds 20 files, which only the walk that crosses finds, and whatever its file
    // system keeps for itself.
    assert!(crossing.entries.files >= staying.entries.files + 20);
    assert!(crossing.entries.directories >= staying.entries.directories);
    staying.check().expect("a consistent profile");
    crossing.check().expect("a consistent profile");
}

// ---------------------------------------------------------------------------------------------
// What the walk does about links, folders that change under it, and trees deeper than it has
// handles for.

/// A link is one entry of its own, and the walk goes no further than the link: it asks nothing of
/// what the link points at. A target that is a FIFO, one behind a folder that cannot be searched,
/// one that is missing, one that runs through a file, and a loop are links and nothing more, and
/// none of them is an error of the walk. A `stat` that followed a link would leave the tree: it
/// can start an automount, or wait on a mount that does not answer.
#[cfg(unix)]
#[test]
fn a_link_is_counted_and_nothing_behind_it_is_touched() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    use nix::{sys::stat::Mode, unistd::mkfifo};

    let scratch = Scratch::new();
    let outside = scratch.join("outside");
    fs::create_dir_all(outside.join("locked")).expect("folders");
    fs::write(outside.join("locked").join("inside"), b"x").expect("a file");
    let fifo = outside.join("fifo");
    mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).expect("a FIFO");
    let root = scratch.join("root");
    fs::create_dir(&root).expect("root");
    fs::write(root.join("file"), b"x").expect("a file");
    symlink(&fifo, root.join("to-a-fifo")).expect("a link");
    symlink(
        outside.join("locked").join("inside"),
        root.join("behind-a-folder-that-cannot-be-searched"),
    )
    .expect("a link");
    symlink("nothing", root.join("to-nothing")).expect("a link");
    symlink("file/inner", root.join("through-a-file")).expect("a link");
    symlink("loop", root.join("loop")).expect("a link");
    let root_user = running_as_root();
    if !root_user {
        fs::set_permissions(outside.join("locked"), fs::Permissions::from_mode(0o000))
            .expect("chmod");
    }

    let shape = walk(&root);
    fs::set_permissions(outside.join("locked"), fs::Permissions::from_mode(0o700)).expect("chmod");

    assert_eq!(shape.entries.symlinks, 5);
    assert_eq!(shape.symbolic_links.count, 5);
    assert_eq!(shape.entries.files, 1);
    assert_eq!(shape.entries.others, 0, "the FIFO is behind a link");
    assert_eq!(shape.entries.total, 6);
    assert_eq!(
        shape.unreadable.errors, 0,
        "nothing behind a link is asked about, so a folder that cannot be searched there is no error"
    );
    assert_eq!(shape.unreadable.directories, 0);
    shape.check().expect("a consistent profile");
}

/// A profile has no field that says whether the target of a link exists: that is a fact about
/// what lies outside the tree, and finding it out is a step out of the tree.
#[test]
fn a_profile_counts_its_symbolic_links_and_says_nothing_of_where_they_point() {
    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir(&root).expect("root");
    fs::write(root.join("file"), b"x").expect("a file");

    let shape = walk(&root);
    let text = shape.to_json_pretty().expect("a document");
    let document: Value = serde_json::from_str(&text).expect("JSON");

    let links = document["symbolic_links"].as_object().expect("an object");
    assert_eq!(
        links.keys().map(String::as_str).collect::<Vec<_>>(),
        ["count"],
        "the fields of the links: {text}"
    );
    assert!(
        !text.contains("dangling"),
        "the profile says nothing of dangling links: {text}"
    );
}

/// Whether a name the walk hands over is `text`.
#[cfg(unix)]
fn is_named(name: &Name, text: &str) -> bool {
    name.as_slice() == text.as_bytes()
}

/// Whether a name the walk hands over is `text`.
#[cfg(windows)]
fn is_named(name: &Name, text: &str) -> bool {
    name.to_str() == Some(text)
}

/// What the walk finds when `change` is done to the tree just before it opens a folder called
/// `called`.
fn walk_changing(
    root: &Path,
    called: &'static str,
    change: impl Fn() + 'static,
) -> HarnessShapeProfile {
    let seam = Seam {
        budget: HANDLE_BUDGET,
        before_open: Some(Box::new(move |_depth, name: &Name| {
            if is_named(name, called) {
                change();
            }
        })),
    };
    let (shape, _, _) = profile_under(root, WalkOptions::default(), seam)
        .unwrap_or_else(|error| panic!("the walk: {error}"));
    shape
}

/// A folder is inspected when its parent is listed and opened later, and what the walk lists is
/// what it opened and not what it inspected: a folder that is not the one that was inspected is
/// counted as having changed, and is not listed.
#[cfg(unix)]
#[test]
fn a_folder_replaced_after_it_was_inspected_is_counted_and_not_listed() {
    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir_all(root.join("a")).expect("folders");
    fs::write(root.join("a").join("original"), b"x").expect("a file");
    fs::create_dir(root.join("b")).expect("a folder");
    fs::write(root.join("b").join("other"), b"x").expect("a file");

    // Nothing changes: both folders are listed.
    let untouched = walk(&root);
    assert_eq!(untouched.entries.files, 2);
    assert_eq!(untouched.unreadable.errors, 0);

    // The folder `a` is replaced by another folder, with three files in it, just before the walk
    // opens it.
    let shape = walk_changing(&root, "a", {
        let root = root.clone();
        move || {
            fs::rename(root.join("a"), root.join("a-aside")).expect("a rename");
            fs::create_dir(root.join("a")).expect("a folder");
            for file in ["one", "two", "three"] {
                fs::write(root.join("a").join(file), b"x").expect("a file");
            }
        }
    });

    assert_eq!(shape.unreadable.errors, 1, "the folder that changed");
    assert_eq!(
        shape.entries.directories, 2,
        "`a` and `b`, as they were inspected"
    );
    assert_eq!(
        shape.entries.files, 1,
        "`other` only: neither the original nor the new files of `a` are listed"
    );
    shape.check().expect("a consistent profile");
}

#[cfg(unix)]
#[test]
fn a_link_put_where_a_folder_was_is_not_followed() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir_all(root.join("a")).expect("folders");
    let elsewhere = scratch.join("elsewhere");
    fs::create_dir(&elsewhere).expect("a folder");
    for file in ["one", "two", "three"] {
        fs::write(elsewhere.join(file), b"x").expect("a file");
    }

    let shape = walk_changing(&root, "a", {
        let (root, elsewhere) = (root.clone(), elsewhere.clone());
        move || {
            fs::remove_dir(root.join("a")).expect("the folder goes");
            symlink(&elsewhere, root.join("a")).expect("a link");
        }
    });

    assert_eq!(shape.unreadable.errors, 1, "the folder that became a link");
    assert_eq!(shape.entries.directories, 1, "`a`, as it was inspected");
    assert_eq!(
        shape.entries.files, 0,
        "what the link points at is not listed"
    );
    shape.check().expect("a consistent profile");
}

/// A junction needs no privilege to make, and a walk that holds a folder cannot have it replaced
/// by one.
#[cfg(windows)]
#[test]
fn a_junction_put_where_a_folder_was_is_not_entered() {
    use std::process::Command;

    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir_all(root.join("a")).expect("folders");
    let elsewhere = scratch.join("elsewhere");
    fs::create_dir(&elsewhere).expect("a folder");
    for file in ["one", "two", "three"] {
        fs::write(elsewhere.join(file), b"x").expect("a file");
    }

    let shape = walk_changing(&root, "a", {
        let (root, elsewhere) = (root.clone(), elsewhere.clone());
        move || {
            fs::remove_dir(root.join("a")).expect("the folder goes");
            let made = Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(root.join("a"))
                .arg(&elsewhere)
                .output()
                .expect("cmd runs");
            assert!(made.status.success(), "a junction is made: {made:?}");
        }
    });

    assert_eq!(
        shape.unreadable.errors, 1,
        "the folder that became a junction"
    );
    assert_eq!(
        shape.entries.files, 0,
        "what the junction points at is not listed"
    );
    shape.check().expect("a consistent profile");
}

/// While the walk is inside a folder, nothing can rename it.
#[cfg(windows)]
#[test]
fn a_folder_the_walk_is_inside_cannot_be_renamed_while_it_runs() {
    use std::{cell::Cell, rc::Rc};

    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir_all(root.join("a").join("b")).expect("folders");
    fs::write(root.join("a").join("b").join("file"), b"x").expect("a file");

    let refused = Rc::new(Cell::new(false));
    let shape = walk_changing(&root, "b", {
        let (root, refused) = (root.clone(), Rc::clone(&refused));
        move || {
            // The walk is inside `a`, which holds `b`.
            refused.set(fs::rename(root.join("a"), root.join("renamed")).is_err());
        }
    });

    assert!(
        refused.get(),
        "`a` cannot be renamed while the walk is inside it"
    );
    assert!(root.join("a").join("b").join("file").exists());
    assert_eq!(shape.entries.files, 1);
    assert_eq!(shape.unreadable.errors, 0);
}

/// A name that is UTF-16 with a surrogate that is half of no pair can be made on Windows, and the
/// walk must reach the entry by that name, not by a lossy form of it that names nothing.
#[cfg(windows)]
#[test]
fn a_name_with_a_lone_surrogate_is_counted_with_its_wtf8_length() {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt as _};

    let scratch = Scratch::new();
    let root = scratch.join("root");
    fs::create_dir(&root).expect("root");
    // `a`, a high surrogate with no partner, and `b`: 1 + 3 + 1 bytes as WTF-8.
    let odd = OsString::from_wide(&[0x61, 0xD800, 0x62]);
    fs::write(root.join(&odd), b"x").expect("a file whose name is not valid Unicode");
    fs::write(root.join("x"), b"x").expect("a file");

    let shape = walk(&root);

    assert_eq!(
        shape.entries.files, 2,
        "the file with the odd name is counted"
    );
    assert_eq!(shape.unreadable.errors, 0, "and reached");
    assert_eq!(
        shape.name_lengths.files.buckets.get(5),
        1,
        "`a`, the surrogate, and `b`: 5 bytes"
    );
    assert_eq!(shape.name_lengths.files.buckets.get(1), 1, "the plain name");
    assert_eq!(shape.name_lengths.files.max, 5);
    shape.check().expect("a consistent profile");
}

/// What the oracle counts for the tree at `root`, held against `actual`.
#[cfg(unix)]
fn assert_same_as_the_oracle(actual: &HarnessShapeProfile, root: &Path, label: &str) {
    let oracle = Oracle::collect(root).expect("the oracle walk");
    let expected = expected_from(&oracle);
    assert_eq!(actual.entries, expected.entries, "{label}: entries by kind");
    assert_eq!(
        actual.max_depth, expected.max_depth,
        "{label}: the deepest level"
    );
    assert_eq!(
        actual.depths, expected.depths,
        "{label}: entries and bytes per depth"
    );
    assert_eq!(
        actual.children_per_directory, expected.children_per_directory,
        "{label}: children per directory"
    );
    assert_eq!(
        actual.subdirectories_per_directory, expected.subdirectories_per_directory,
        "{label}: folders per directory"
    );
    assert_eq!(
        actual.files_per_directory, expected.files_per_directory,
        "{label}: files per directory"
    );
    assert_eq!(
        actual.file_sizes, expected.file_sizes,
        "{label}: file sizes"
    );
    assert_eq!(
        actual.name_lengths, expected.name_lengths,
        "{label}: name lengths"
    );
    assert_eq!(
        actual.unreadable, expected.unreadable,
        "{label}: unreadable entries"
    );
}

/// A walk under a handle budget of its own: its profile, the most handles it had open at once,
/// and how many it opened in all.
#[cfg(unix)]
fn under(root: &Path, budget: usize) -> (HarnessShapeProfile, usize, usize) {
    let seam = Seam {
        budget,
        before_open: None,
    };
    profile_under(root, WalkOptions::default(), seam)
        .unwrap_or_else(|error| panic!("the walk under a budget of {budget}: {error}"))
}

/// A chain of `levels` folders called `d`, one inside the other, with a file `f` of as many bytes
/// as its level in the folder at every 50th level. It takes itself down when it drops, from the
/// deepest folder up and by path, so that no handle is held for each level: `Scratch` removes a
/// tree with a handle open for every level it is inside, and a shell's soft limit on descriptors,
/// a macOS shell's 256, is lower than 300, so it would leave the tree behind and, for as long as
/// it tried, leave the tests that run beside it with no descriptor to open a folder with.
#[cfg(unix)]
struct Chain {
    root: PathBuf,
    levels: usize,
}

#[cfg(unix)]
impl Chain {
    fn new(scratch: &Scratch, levels: usize) -> Self {
        let root = scratch.join("chain");
        let mut folder = root.clone();
        for _ in 0..levels {
            folder.push("d");
        }
        fs::create_dir_all(&folder).expect("a chain of folders");
        let mut folder = root.clone();
        for level in 1..=levels {
            folder.push("d");
            if level % 50 == 0 {
                fs::write(folder.join("f"), vec![0_u8; level]).expect("a file");
            }
        }
        Self { root, levels }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

#[cfg(unix)]
impl Drop for Chain {
    fn drop(&mut self) {
        let mut folder = self.root.clone();
        for _ in 0..self.levels {
            folder.push("d");
        }
        for _ in 0..self.levels {
            let _ = fs::remove_file(folder.join("f"));
            let _ = fs::remove_dir(&folder);
            folder.pop();
        }
    }
}

/// A comb: a spine of `levels` folders called `m`, one inside the other, and in the folder at each
/// level one folder for each of `teeth`, each holding a file `x`.
#[cfg(unix)]
fn comb(scratch: &Scratch, name: &str, levels: usize, teeth: &[&str]) -> PathBuf {
    let root = scratch.join(name);
    fs::create_dir(&root).expect("root");
    let mut level = root.clone();
    for _ in 0..levels {
        for tooth in teeth {
            fs::create_dir(level.join(tooth)).expect("a tooth");
            fs::write(level.join(tooth).join("x"), b"x").expect("a file");
        }
        level.push("m");
        fs::create_dir(&level).expect("the spine");
    }
    root
}

/// A tree deeper than a shell's soft limit on descriptors, a macOS shell's 256, is walked in full
/// by a walk that is allowed four folder handles, and no more than four are ever open. A folder
/// that has no subfolder left lets go of its handle, so a chain never has to open one again.
#[cfg(unix)]
#[test]
fn a_chain_of_300_folders_is_walked_in_full_within_a_budget_of_four_handles() {
    let scratch = Scratch::new();
    let chain = Chain::new(&scratch, 300);
    let root = chain.root();

    let (small, peak, opened) = under(root, 4);
    let (default, default_peak, _) = under(root, HANDLE_BUDGET);

    assert_eq!(small.entries.directories, 300);
    assert_eq!(small.entries.files, 6, "a file at every 50th level");
    assert_eq!(
        small.max_depth, 301,
        "the files at level 300 are one deeper"
    );
    assert_eq!(small.unreadable.errors + small.unreadable.directories, 0);
    assert!(peak <= 4, "{peak} handles were open at once");
    assert!(default_peak <= HANDLE_BUDGET, "{default_peak} handles");
    assert_eq!(
        opened, 301,
        "each folder and the root, once each: none was opened again"
    );
    assert_eq!(small, default, "the budget changes nothing the walk finds");
    small.check().expect("a consistent profile");
}

/// A comb makes the walk give up the handles of folders that still have a subfolder to enter, so
/// it has to open them again, from the nearest folder it still holds, and what it finds is what
/// the oracle finds, and what a walk with no budget finds.
#[cfg(unix)]
#[test]
fn a_comb_is_walked_in_full_within_a_small_budget_by_opening_folders_again() {
    let scratch = Scratch::new();
    for (name, teeth) in [("comb-two", &["t"][..]), ("comb-three", &["a", "z"])] {
        let root = comb(&scratch, name, 100, teeth);

        let (unbounded, unbounded_peak, unbounded_opened) = under(&root, 10_000);
        let (small, small_peak, small_opened) = under(&root, 4);
        let (default, default_peak, _) = under(&root, HANDLE_BUDGET);

        assert_same_as_the_oracle(&small, &root, name);
        assert_eq!(
            small, unbounded,
            "{name}: a budget of four changes nothing the walk finds"
        );
        assert_eq!(default, unbounded, "{name}: nor does the default one");
        assert_eq!(
            small.unreadable.errors + small.unreadable.directories,
            0,
            "{name}"
        );
        assert!(
            small_peak <= 4,
            "{name}: {small_peak} handles were open at once"
        );
        assert!(
            default_peak <= HANDLE_BUDGET,
            "{name}: {default_peak} handles"
        );
        if teeth.len() == 2 {
            // A spine that is in the middle of the folders of its level is entered with another
            // folder still to enter, whatever order the file system lists them in, so with no
            // budget the walk holds a handle for most of the levels, and with one it opens
            // folders again.
            assert!(
                unbounded_peak > HANDLE_BUDGET,
                "{name}: {unbounded_peak} handles with no budget"
            );
            assert!(
                small_opened > unbounded_opened,
                "{name}: {small_opened} handles opened, against {unbounded_opened} with no budget"
            );
        }
    }
}

/// A full binary tree of folders called `0` and `1`, `levels` deep, with a file `f` in each of
/// the folders at the last level.
#[cfg(unix)]
fn binary_tree(scratch: &Scratch, name: &str, levels: usize) -> PathBuf {
    let root = scratch.join(name);
    fs::create_dir(&root).expect("root");
    let mut level = vec![root.clone()];
    for depth in 1..=levels {
        let mut next = Vec::new();
        for parent in &level {
            for child in ["0", "1"] {
                let folder = parent.join(child);
                fs::create_dir(&folder).expect("a folder");
                if depth == levels {
                    fs::write(folder.join("f"), b"x").expect("a file");
                }
                next.push(folder);
            }
        }
        level = next;
    }
    root
}

/// A folder whose handle was given up is opened again by name from the nearest one the walk still
/// holds, and it has to be the folder that was recorded: when the walk goes back to a folder
/// below one that was replaced in the meantime, it counts the change and does not walk what the
/// folder still had to enter. A walk with no budget never gave a handle up, so it never notices.
#[cfg(unix)]
#[test]
fn a_folder_that_is_not_the_one_recorded_is_not_walked_when_it_is_opened_again() {
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    let scratch = Scratch::new();
    let full = walk(&binary_tree(&scratch, "reference", 6));
    assert_eq!((full.entries.directories, full.entries.files), (126, 64));

    // The first time the walk opens a folder four levels down, the folder two levels down on its
    // way is replaced by another one, empty.
    let changing = |budget: usize, name: &str| {
        let root = binary_tree(&scratch, name, 6);
        let way: Rc<RefCell<Vec<String>>> = Rc::default();
        let done = Rc::new(Cell::new(false));
        let seam = Seam {
            budget,
            before_open: Some(Box::new({
                let root = root.clone();
                move |depth, name: &Name| {
                    let mut way = way.borrow_mut();
                    way.truncate(usize::try_from(depth - 1).expect("a small depth"));
                    way.push(String::from_utf8(name.clone()).expect("a name of digits"));
                    if depth == 4 && !done.replace(true) {
                        let second = root.join(&way[0]).join(&way[1]);
                        fs::rename(&second, second.with_file_name("aside")).expect("a rename");
                        fs::create_dir(&second).expect("a folder");
                    }
                }
            })),
        };
        let (shape, _, _) = profile_under(&root, WalkOptions::default(), seam).expect("the walk");
        shape
    };

    let small = changing(4, "small");
    assert!(
        small.unreadable.errors >= 1,
        "a folder that was not the one recorded was counted: {:?}",
        small.unreadable
    );
    assert!(
        small.entries.files < full.entries.files,
        "what was left to enter below the folder that changed was not walked: {} of {} files",
        small.entries.files,
        full.entries.files
    );
    small.check().expect("a consistent profile");

    let unbounded = changing(10_000, "unbounded");
    assert_eq!(
        unbounded.unreadable.errors, 0,
        "no handle was given up, so none was opened again"
    );
    assert_eq!(unbounded.entries.files, full.entries.files);
}

// ---------------------------------------------------------------------------------------------
// The fixture a profile builds, and the tree that fixture makes.

/// A small deterministic generator for the trees the tests build.
#[cfg(unix)]
struct Lcg(u64);

#[cfg(unix)]
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 16
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// A count that is mostly small and sometimes large: `2^k` to `2^(k + 1) - 1`, where `k` is
    /// the number of trailing zeros of a random number, at most `most`.
    fn heavy(&mut self, most: u32) -> u64 {
        let class = self.next().trailing_zeros().min(most);
        (1 << class) + self.below(1 << class)
    }
}

/// A name of between `low` and `high` bytes that no other `counter` makes.
#[cfg(unix)]
fn name_of(rng: &mut Lcg, counter: u64, low: u64, high: u64) -> String {
    let mut name = format!("{counter:x}");
    let wanted = usize::try_from(low + rng.below(high - low + 1)).expect("a small length");
    while name.len() < wanted {
        name.push(char::from(
            b'a' + u8::try_from(rng.below(26)).expect("a letter"),
        ));
    }
    name
}

/// Builds a tree that looks like a home folder in miniature, where a few folders hold most of the
/// files: seven levels deep, one wide folder, files from a byte to 32 KiB (made sparse, so that
/// the test writes nothing), links that resolve and links that do not, and files with several
/// names, in four classes of size, in groups of two to nine names.
#[cfg(unix)]
fn build_home(root: &Path) {
    use std::os::unix::fs::symlink;

    let mut rng = Lcg(0x5eed);
    let mut counter = 0_u64;
    let mut files: Vec<PathBuf> = Vec::new();
    // The class of the size of each of `files`: a file of class `n` is of `2^n` to `2^(n+1) - 1`
    // bytes.
    let mut file_classes: Vec<u64> = Vec::new();
    let mut folders: Vec<(PathBuf, u32)> = vec![(root.to_path_buf(), 0)];
    let mut at = 0;
    while at < folders.len() {
        let (folder, level) = folders[at].clone();
        at += 1;
        if level < 7 {
            // A real home is bushy near the top and thins out: the root holds thirty folders,
            // the next levels a few each, and the deeper ones fewer than one on average. The
            // first folder of a level always has a subfolder, so the tree is as deep as it is
            // meant to be.
            let first = folders.iter().all(|(_, other)| *other != level + 1);
            let wanted = match level {
                0 => 30,
                1 | 2 => rng.heavy(3) - 1,
                _ => rng.heavy(1) - 1,
            };
            for _ in 0..if first { wanted.max(1) } else { wanted } {
                counter += 1;
                let path = folder.join(name_of(&mut rng, counter, 2, 18));
                fs::create_dir(&path).expect("a folder");
                folders.push((path, level + 1));
            }
        }
        // One folder is very wide; the others hold a few files, or a lot; and every folder of the
        // deepest level holds one, so that the tree is as deep as it is meant to be.
        let count = if at == 4 { 700 } else { rng.heavy(6) - 1 };
        let count = if level == 7 { count.max(1) } else { count };
        for _ in 0..count {
            counter += 1;
            let path = folder.join(name_of(&mut rng, counter, 3, 30));
            // Sizes from a byte to 32 KiB, every class as likely as another.
            let class = rng.below(15);
            let size = (1 << class) + rng.below(1 << class);
            let file = fs::File::create(&path).expect("a file");
            file.set_len(size).expect("a length");
            files.push(path);
            file_classes.push(class);
        }
    }
    // Links: forty, of which a quarter dangle.
    for index in 0..40_u64 {
        let folder = &folders[usize::try_from(rng.below(folders.len() as u64)).expect("index")].0;
        counter += 1;
        let link = folder.join(name_of(&mut rng, counter, 4, 14));
        if index % 4 == 0 {
            symlink(format!("gone-{index}"), link).expect("a link");
        } else {
            let target = &files[usize::try_from(rng.below(files.len() as u64)).expect("index")];
            symlink(target, link).expect("a link");
        }
    }
    // Hard links, in four classes of size: each class has groups of two names and a few of more,
    // so that the classes differ in how many groups they have and in how large they are.
    for (class, groups) in [
        (2_u64, &[2_u64, 2, 3, 2, 2, 5, 2, 2][..]),
        (5, &[2, 3, 2, 4, 2, 2, 6, 3, 2, 2][..]),
        (8, &[2, 2, 2, 2, 9, 3, 2, 2, 4, 2, 2, 2][..]),
        (11, &[3, 2, 2, 8, 2, 3][..]),
    ] {
        // The files of the class, in a random order, a file for each group.
        let mut candidates: Vec<usize> = (0..files.len())
            .filter(|index| file_classes[*index] == class)
            .collect();
        assert!(
            candidates.len() >= groups.len(),
            "files enough in the class {class}"
        );
        for (group, names) in groups.iter().enumerate() {
            let pick = group
                + usize::try_from(rng.below((candidates.len() - group) as u64)).expect("index");
            candidates.swap(group, pick);
            let original = files[candidates[group]].clone();
            for extra in 1..*names {
                let folder =
                    &folders[usize::try_from(rng.below(folders.len() as u64)).expect("index")].0;
                counter += 1;
                let name = name_of(&mut rng, counter, 3 + extra % 5, 25);
                fs::hard_link(&original, folder.join(name)).expect("a hard link");
            }
        }
    }
}

/// `part` as a fraction of `whole`. The counts of a test tree are far below 2^52.
#[cfg(unix)]
#[allow(clippy::cast_precision_loss)]
fn ratio(part: u64, whole: u64) -> f64 {
    part as f64 / whole as f64
}

/// The share of each key in a histogram, in the order of the keys.
#[cfg(unix)]
fn shares(histogram: &ShapeHistogram) -> BTreeMap<u64, f64> {
    histogram
        .buckets
        .iter()
        .map(|(key, count)| (key, ratio(count, histogram.count)))
        .collect()
}

/// The total variation distance between two histograms: half the sum of the differences of their
/// shares, 0 for the same shape and 1 for shapes that share nothing.
#[cfg(unix)]
fn distance(left: &ShapeHistogram, right: &ShapeHistogram) -> f64 {
    let (left, right) = (shares(left), shares(right));
    let keys: Vec<u64> = left.keys().chain(right.keys()).copied().collect();
    let mut seen = Vec::new();
    let mut sum = 0.0;
    for key in keys {
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        sum += (left.get(&key).unwrap_or(&0.0) - right.get(&key).unwrap_or(&0.0)).abs();
    }
    sum / 2.0
}

/// How much of the entries of a tree are at each depth.
#[cfg(unix)]
fn depth_shares(shape: &HarnessShapeProfile) -> Vec<f64> {
    shape
        .depths
        .iter()
        .map(|level| {
            ratio(
                level.directories + level.files + level.symlinks + level.others,
                shape.entries.total,
            )
        })
        .collect()
}

/// The tolerances the round trip is held to, and what each one is for. A profile scaled to a
/// number of entries gives a tree whose shape agrees with the profile's within these; they are
/// the ones the harness README states. They were set from the distances this test measures
/// (about 0.004 for a kind's share, 0.001 for a depth's, 0.1 for what a folder holds, 0.006 for
/// file sizes, 0.012 for name lengths, 0.022 for the share of a class's files that are names of
/// linked files and no group of a size more than the rounding away from the profile's count
/// scaled) with room to spare, and not from what the generator could be made to pass.
#[cfg(unix)]
mod tolerance {
    /// Entries of the generated tree against the entries asked for: exactly (the marker is the
    /// one more).
    pub const ENTRIES: u64 = 1;
    /// The share of folders, files, and links among the entries, in absolute terms.
    pub const KIND_SHARE: f64 = 0.01;
    /// The share of the entries at each depth, in absolute terms for each depth.
    pub const DEPTH_SHARE: f64 = 0.005;
    /// The total variation distance of the histograms of what a folder holds. Each level of the
    /// tree shares its entries among its folders in proportion to weights from the histogram,
    /// which stretches the histogram by that level's mean, so the whole tree keeps the shape of
    /// the histogram only in outline: the zeros and the tail are there, and the classes between
    /// them move a little.
    pub const PER_DIRECTORY: f64 = 0.15;
    /// The same for file sizes, which every file draws from, so that the shape holds at any scale.
    pub const FILE_SIZES: f64 = 0.02;
    /// The same for name lengths, which a folder cannot always honor: a name has the digits that
    /// tell a folder's entries apart before any length it was drawn.
    pub const NAME_LENGTHS: f64 = 0.03;
    /// A histogram of fewer values than this is not compared: too few to have a shape.
    pub const SMALLEST_HISTOGRAM: u64 = 50;
    /// The share of a class's files that are names of hard-linked files, in absolute terms, with
    /// `1 / files` more for the rounding of a count. The names of a class are scaled with its
    /// files and kept within what its groups can have, which moves them by a rounding of each
    /// size of group.
    pub const LINKED_SHARE: f64 = 0.03;
    /// The groups of each size in a class: the profile's count scaled by the files of the class
    /// and rounded, give or take this many.
    pub const GROUPS_OF_A_SIZE: u64 = 1;
}

#[cfg(unix)]
fn request(entries: u64, seed: u64) -> SpecRequest {
    SpecRequest {
        id: "shaped-test".to_owned(),
        entries,
        seed,
        max_file_bytes: 1 << 20,
    }
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn a_profile_builds_a_spec_that_builds_a_tree_of_the_same_shape() {
    let scratch = Scratch::new();
    let home = scratch.join("home");
    fs::create_dir(&home).expect("a folder");
    build_home(&home);
    let original = walk(&home);
    original.check().expect("a consistent profile");
    assert!(original.entries.total > 1_500, "{:?}", original.entries);
    assert_eq!(
        original.max_depth, 8,
        "seven levels of folders and their files"
    );
    assert!(original.hard_links.groups >= 15 && original.symbolic_links.count == 40);

    for entries in [
        original.entries.total / 3,
        original.entries.total,
        3 * original.entries.total,
    ] {
        let spec = spec_from_profile(&original, &request(entries, 3)).expect("a spec");
        assert_eq!(
            spec.planned_entry_count(),
            entries,
            "the spec plans exactly the entries asked for"
        );
        let master = master_of(&scratch, &spec);
        let copy = walk(&master.root);
        copy.check().expect("a consistent profile");
        let label = format!("scaled to {entries} from {}", original.entries.total);

        // The entries: what was asked for, and the marker.
        assert!(
            copy.entries.total.abs_diff(entries) <= tolerance::ENTRIES,
            "{label}: {} entries",
            copy.entries.total
        );
        // The kinds, as shares.
        let share = |shape: &HarnessShapeProfile, count: u64| ratio(count, shape.entries.total);
        for (kind, from, to) in [
            (
                "folders",
                original.entries.directories,
                copy.entries.directories,
            ),
            ("files", original.entries.files, copy.entries.files),
            ("links", original.entries.symlinks, copy.entries.symlinks),
        ] {
            let difference = (share(&original, from) - share(&copy, to)).abs();
            assert!(
                difference <= tolerance::KIND_SHARE,
                "{label}: the share of {kind} is {} and was {}",
                share(&copy, to),
                share(&original, from)
            );
        }
        // The depth, level by level. The root of the part is one level below the root of the
        // fixture, so the copy is one level deeper, and its first level holds the part's root and
        // the marker.
        assert_eq!(copy.max_depth, original.max_depth + 1, "{label}: depth");
        for (index, (was, now)) in depth_shares(&original)
            .into_iter()
            .zip(depth_shares(&copy).into_iter().skip(1))
            .enumerate()
        {
            assert!(
                (was - now).abs() <= tolerance::DEPTH_SHARE,
                "{label}: depth {} holds {now} of the entries and held {was}",
                index + 1
            );
        }
        // The histograms.
        for (what, was, now, limit) in [
            (
                "children per directory",
                &original.children_per_directory,
                &copy.children_per_directory,
                tolerance::PER_DIRECTORY,
            ),
            (
                "folders per directory",
                &original.subdirectories_per_directory,
                &copy.subdirectories_per_directory,
                tolerance::PER_DIRECTORY,
            ),
            (
                "files per directory",
                &original.files_per_directory,
                &copy.files_per_directory,
                tolerance::PER_DIRECTORY,
            ),
            (
                "file sizes",
                &original.file_sizes,
                &copy.file_sizes,
                tolerance::FILE_SIZES,
            ),
            (
                "folder names",
                &original.name_lengths.directories,
                &copy.name_lengths.directories,
                tolerance::NAME_LENGTHS,
            ),
            (
                "file names",
                &original.name_lengths.files,
                &copy.name_lengths.files,
                tolerance::NAME_LENGTHS,
            ),
            (
                "link names",
                &original.name_lengths.symlinks,
                &copy.name_lengths.symlinks,
                tolerance::NAME_LENGTHS,
            ),
        ] {
            // A histogram of fewer values than that is not compared.
            if now.count < tolerance::SMALLEST_HISTOGRAM {
                continue;
            }
            // A histogram of `n` values cannot be closer to a larger one than about `1 / n`.
            let limit = limit + ratio(1, now.count);
            let apart = distance(was, now);
            assert!(
                apart <= limit,
                "{label}: {what} are {apart} apart, at most {limit}"
            );
        }
        // Links: a profile says nothing of where a link pointed, so every link of the spec points
        // at a file of the part, and none dangles.
        assert_eq!(
            copy.symbolic_links.count, copy.entries.symlinks,
            "{label}: links"
        );
        assert!(
            master
                .plan()
                .entries()
                .iter()
                .filter(|entry| entry.kind == NodeKind::Symlink)
                .all(|entry| entry
                    .target
                    .as_ref()
                    .is_some_and(|target| target.as_bytes() != b"missing")),
            "{label}: every link points at a file"
        );
        // Groups of hard links, class of file size by class: the files with several names are the
        // same share of the class, and each size of group is the profile's count scaled with the
        // files of the class. A size of group that the profile did not have is not made.
        assert!(
            original.hard_links.group_sizes_by_file_size.len() >= 4,
            "the home has groups in four classes of size"
        );
        for (class, was) in original.hard_links.group_sizes_by_file_size.iter() {
            let (files_was, files_now) = (
                original.file_sizes.buckets.get(class),
                copy.file_sizes.buckets.get(class),
            );
            let now = copy
                .hard_links
                .group_sizes_by_file_size
                .get(class)
                .unwrap_or_else(|| panic!("{label}: no groups in the class {class}"));
            let (share_was, share_now) = (ratio(was.total, files_was), ratio(now.total, files_now));
            let limit = tolerance::LINKED_SHARE + ratio(1, files_now);
            assert!(
                (share_was - share_now).abs() <= limit,
                "{label}: in the class {class}, {share_now} of the files are names of linked files, \
                 and {share_was} were (at most {limit} apart)"
            );
            for (size, count) in was.buckets.iter() {
                let expected = (count * files_now + files_was / 2) / files_was;
                assert!(
                    now.buckets.get(size).abs_diff(expected) <= tolerance::GROUPS_OF_A_SIZE,
                    "{label}: in the class {class}, {} groups of {size} to {} names, and {expected} \
                     were wanted",
                    now.buckets.get(size),
                    2 * size - 1
                );
            }
            for (size, _) in now.buckets.iter() {
                assert!(
                    was.buckets.get(size) > 0,
                    "{label}: in the class {class}, groups of {size} names, which the profile did \
                     not have"
                );
            }
        }
        assert_eq!(copy.hard_links.incomplete_groups, 0);
        assert_eq!(copy.unreadable.directories + copy.unreadable.errors, 0);
    }
}

#[cfg(unix)]
#[test]
fn the_same_profile_request_and_seed_give_the_same_spec_and_manifest_hash() {
    let scratch = Scratch::new();
    let home = scratch.join("home");
    fs::create_dir(&home).expect("a folder");
    build_home(&home);
    let original = walk(&home);

    let first = spec_from_profile(&original, &request(1_200, 7)).expect("a spec");
    let again = spec_from_profile(&original, &request(1_200, 7)).expect("a spec");
    let other_seed = spec_from_profile(&original, &request(1_200, 8)).expect("a spec");
    let other_size = spec_from_profile(&original, &request(1_201, 7)).expect("a spec");

    assert_eq!(first, again);
    assert_eq!(
        render_spec(&first).expect("TOML"),
        render_spec(&again).expect("TOML"),
        "the text is the same too"
    );
    let hash = |spec: &FixtureSpec| {
        Plan::new(spec)
            .expect("a plan")
            .manifest_sha256()
            .to_owned()
    };
    assert_eq!(hash(&first), hash(&again));
    assert_ne!(
        hash(&first),
        hash(&other_seed),
        "the seed shapes the tree's details"
    );
    assert_ne!(hash(&first), hash(&other_size));

    // The seed decides who gets which share and which size, and not how many of anything there is.
    let counts = |spec: &FixtureSpec| {
        let plan = Plan::new(spec).expect("a plan");
        [NodeKind::Directory, NodeKind::File, NodeKind::Symlink].map(|kind| {
            plan.entries()
                .iter()
                .filter(|entry| entry.kind == kind)
                .count()
        })
    };
    assert_eq!(counts(&first), counts(&other_seed));

    // The generated tree has the manifest hash the plan has, on every machine.
    let master = master_of(&scratch, &first);
    assert_eq!(master.manifest_sha256(), hash(&first));
    let again = master_of(&scratch, &again);
    assert!(again.cache_hit, "the same spec is the same cache entry");
}

#[cfg(unix)]
#[test]
fn a_rendered_spec_parses_back_to_the_spec_and_names_nothing() {
    let scratch = Scratch::new();
    let home = scratch.join("home");
    fs::create_dir(&home).expect("a folder");
    build_home(&home);
    let original = walk(&home);
    let spec = spec_from_profile(&original, &request(900, 1)).expect("a spec");

    let text = render_spec(&spec).expect("TOML");
    let back = FixtureSpec::from_toml_str(&text).expect("a spec file parses");

    assert_eq!(back, spec);
    assert_eq!(back.spec_hash(), spec.spec_hash());
    assert!(text.starts_with("# A fixture specification built by `excise-shape spec`"));
    assert!(
        text.contains("kind = \"shaped\"") && text.contains(&format!("root = \"{SHAPED_ROOT}\""))
    );
    assert!(!text.contains(DISTINCTIVE));
    assert!(spec.description.starts_with("Shaped like a profile of "));
}

#[test]
fn a_spec_cannot_be_built_from_what_cannot_be_built_on() {
    let scratch = Scratch::new();
    let empty = walk(scratch.path());
    assert!(matches!(
        spec_from_profile(
            &empty,
            &SpecRequest {
                id: "x".to_owned(),
                entries: 10,
                seed: 1,
                max_file_bytes: 1
            }
        ),
        Err(DeriveError::Empty)
    ));

    fs::create_dir_all(scratch.join("a").join("b").join("c").join("d")).expect("a chain");
    let chain = walk(&scratch.join("a"));
    assert_eq!(chain.max_depth, 3);
    let ask = |entries| SpecRequest {
        id: "chain".to_owned(),
        entries,
        seed: 1,
        max_file_bytes: 1,
    };
    assert!(matches!(
        spec_from_profile(&chain, &ask(0)),
        Err(DeriveError::Entries { .. })
    ));
    assert!(matches!(
        spec_from_profile(&chain, &ask(10_100_001)),
        Err(DeriveError::Entries { .. })
    ));
    assert!(matches!(
        spec_from_profile(&chain, &ask(3)),
        Err(DeriveError::TooSmall {
            depth: 3,
            minimum: 4,
            ..
        })
    ));
    // Just enough keeps the whole chain.
    let spec = spec_from_profile(&chain, &ask(4)).expect("a spec");
    assert_eq!(spec.planned_entry_count(), 4);
    let master = master_of(&scratch, &spec);
    assert_eq!(
        walk(&master.root).max_depth,
        1 + 3,
        "the part's root adds a level"
    );

    // A profile whose numbers disagree is refused, whoever wrote it.
    let mut lying = chain.clone();
    lying.entries.files += 1;
    assert!(matches!(
        spec_from_profile(&lying, &ask(10)),
        Err(DeriveError::Inconsistent(_))
    ));
    // A spec that is too large for the generator is refused by the spec's own rules.
    let mut over = request_for_depth(&chain);
    over.max_file_bytes = u64::MAX;
    assert!(matches!(
        spec_from_profile(&chain, &over),
        Err(DeriveError::Invalid(_))
    ));
}

fn request_for_depth(shape: &HarnessShapeProfile) -> SpecRequest {
    SpecRequest {
        id: "chain".to_owned(),
        entries: u64::from(shape.max_depth) + 5,
        seed: 1,
        max_file_bytes: 1,
    }
}

#[test]
fn a_tree_deeper_than_a_shaped_part_allows_is_folded_into_its_last_level() {
    let scratch = Scratch::new();
    // A chain of 40 folders, each holding one file.
    let mut folder = scratch.join("deep");
    fs::create_dir(&folder).expect("a folder");
    let top = folder.clone();
    for level in 0..39 {
        fs::write(folder.join("file"), b"x").expect("a file");
        folder = folder.join(format!("d{level}"));
        fs::create_dir(&folder).expect("a folder");
    }
    let deep = walk(&top);
    assert_eq!(
        deep.max_depth, 39,
        "a file in each of 39 levels, the last also a folder"
    );

    let spec = spec_from_profile(
        &deep,
        &SpecRequest {
            id: "folded".to_owned(),
            entries: 200,
            seed: 1,
            max_file_bytes: 64,
        },
    )
    .expect("a spec");

    let Some(crate::fixture::Part::Shaped(part)) = spec.parts.first() else {
        panic!("one shaped part");
    };
    assert_eq!(part.levels.len(), 32, "the deepest a shaped part goes");
    assert!(spec.description.contains("deeper than 32 levels is at 32"));
    assert_eq!(spec.planned_entry_count(), 200);
    let master = master_of(&scratch, &spec);
    assert_eq!(walk(&master.root).max_depth, 33);
}

#[test]
fn the_profile_of_a_generated_fixture_validates_against_its_schema_and_rejects_unknown_fields() {
    let schema: Value =
        serde_json::from_str(HarnessShapeProfile::SCHEMA_JSON).expect("the schema is JSON");
    let validator = jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("the schema compiles");
    let scratch = Scratch::new();

    for id in ORACLE_FIXTURES {
        let shape = walk(&bundled_master(&scratch, id).root);
        let text = shape.to_json_pretty().expect("a document");
        let document: Value = serde_json::from_str(&text).expect("JSON");

        let violations: Vec<String> = validator
            .iter_errors(&document)
            .map(|error| error.to_string())
            .collect();
        assert!(violations.is_empty(), "{id}: {violations:#?}");
        assert_eq!(
            HarnessShapeProfile::from_json_str(&text).expect("the document reads back"),
            shape,
            "{id}"
        );

        // A field that is not the schema's is refused by the schema and by the type, at the top
        // and in every object below it.
        let mut pointers: Vec<String> = [
            "",
            "/platform",
            "/walk",
            "/entries",
            "/depths/0",
            "/file_sizes",
            "/name_lengths",
            "/name_lengths/files",
            "/hard_links",
            "/hard_links/group_sizes",
            "/hard_links/group_sizes_by_file_size",
            "/symbolic_links",
            "/unreadable",
        ]
        .map(String::from)
        .to_vec();
        // And in the histogram of each class of groups, where the fixture has any.
        for class in shape
            .hard_links
            .group_sizes_by_file_size
            .iter()
            .map(|(class, _)| class)
        {
            pointers.push(format!("/hard_links/group_sizes_by_file_size/{class}"));
        }
        for pointer in &pointers {
            let mut extra = document.clone();
            extra
                .pointer_mut(pointer)
                .and_then(Value::as_object_mut)
                .unwrap_or_else(|| panic!("{id}: {pointer} is an object"))
                .insert("owner".to_owned(), 501.into());
            assert!(
                !validator.is_valid(&extra),
                "{id}: the schema accepts a field at `{pointer}`"
            );
            assert!(
                serde_json::from_value::<HarnessShapeProfile>(extra).is_err(),
                "{id}: the type accepts a field at `{pointer}`"
            );
        }
    }
}

/// The names of a hard-linked file are one file, so the groups of a class are among the files of
/// that class. A profile scaled down so far that the files of a class cannot hold its group
/// leaves the group out, and the spec still builds: a spec never asks for more names than a class
/// has files. Scaled to what holds it, the plan has the group, with both names in the class they
/// were in.
#[cfg(unix)]
#[test]
fn a_profile_scaled_down_until_a_class_cannot_hold_its_group_leaves_the_group_out() {
    use crate::fixture::spec::Part;

    let scratch = Scratch::new();
    let home = scratch.join("home");
    fs::create_dir(&home).expect("a folder");
    // Five files in four classes of size, two of them names of one file.
    fs::write(home.join("a"), b"x").expect("a file");
    fs::hard_link(home.join("a"), home.join("b")).expect("a hard link");
    fs::write(home.join("c"), vec![0_u8; 100]).expect("a file");
    fs::write(home.join("d"), vec![0_u8; 5000]).expect("a file");
    fs::write(home.join("e"), vec![0_u8; 70_000]).expect("a file");
    let shape = walk(&home);
    assert_eq!((shape.entries.files, shape.hard_links.groups), (5, 1));

    // Three files, one in each of three classes: no class holds a group of two names.
    let spec = spec_from_profile(&shape, &request(4, 1)).expect("a spec");
    let Part::Shaped(part) = &spec.parts[0] else {
        panic!("a shaped part");
    };
    assert!(part.hard_links.is_empty(), "{:?}", part.hard_links);
    Plan::new(&spec).expect("a plan");

    // All five files: the two names are placed in the class that holds two.
    let spec = spec_from_profile(&shape, &request(6, 1)).expect("a spec");
    for seed in 1..=4 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        let names: Vec<u64> = plan
            .entries()
            .iter()
            .filter(|entry| entry.link_group.is_some())
            .map(|entry| entry.size)
            .collect();
        assert_eq!(
            names,
            [1, 1],
            "seed {seed}: one group of two names of 1 byte"
        );
    }
}

/// A tree of `sizes`: for each size of file, the number of names of each of its files, as a file
/// and the links to it, all in one folder. Returns the folder.
#[cfg(unix)]
fn grouped_tree(scratch: &Scratch, groups: &[(usize, &[usize])]) -> PathBuf {
    let home = scratch.join("home");
    fs::create_dir(&home).expect("a folder");
    for (size, names) in groups {
        for (group, count) in names.iter().enumerate() {
            let first = home.join(format!("{size}-{group}-0"));
            fs::write(&first, vec![0_u8; *size]).expect("a file");
            for name in 1..*count {
                fs::hard_link(&first, home.join(format!("{size}-{group}-{name}")))
                    .expect("a hard link");
            }
        }
    }
    home
}

/// The groups of `plan` by the class of the size of their file: how many names each has, in
/// ascending order.
#[cfg(unix)]
fn group_names_by_class(plan: &Plan) -> BTreeMap<u64, Vec<usize>> {
    let mut names: BTreeMap<u32, (u64, usize)> = BTreeMap::new();
    for entry in plan.entries() {
        if let Some(group) = entry.link_group {
            let slot = names.entry(group).or_insert((class(entry.size), 0));
            slot.1 += 1;
        }
    }
    let mut by_class: BTreeMap<u64, Vec<usize>> = BTreeMap::new();
    for (file_class, count) in names.into_values() {
        by_class.entry(file_class).or_default().push(count);
    }
    for counts in by_class.values_mut() {
        counts.sort_unstable();
    }
    by_class
}

/// Four files of 100 bytes, each with three more names: sixteen files in four groups of four
/// names. The one histogram of group sizes says four groups in the class of 4 to 7 and nothing of
/// how many names they have in all, so a spec built from that alone would spread them over 4 to
/// 7 and ask for more names than the profile had (22, for 17 entries, and refuse). The profile
/// carries the names of the class, and the spec built from it at 17 entries is four groups of
/// four names again.
#[cfg(unix)]
#[test]
fn four_groups_of_four_names_are_four_groups_of_four_names_again_at_17_entries() {
    let scratch = Scratch::new();
    let home = grouped_tree(&scratch, &[(100, &[4, 4, 4, 4])]);
    let shape = walk(&home);
    assert_eq!(
        (
            shape.entries.files,
            shape.hard_links.groups,
            shape.hard_links.names
        ),
        (16, 4, 16)
    );

    let spec = spec_from_profile(&shape, &request(17, 1)).expect("a spec the files can hold");
    for seed in 1..=6 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        assert_eq!(
            group_names_by_class(&plan),
            BTreeMap::from([(64, vec![4, 4, 4, 4])]),
            "seed {seed}"
        );
    }
}

/// Thirty files in three classes of size, ten in each, in ten groups of 6, 2, 2 and 4, 3, 3 and
/// 3, 3, 2, 2 names. Taken together they are eight groups of 2 or 3 names and two of 4 to 7,
/// which a rule that places each group wherever it fits, largest first, cannot pack into three
/// classes of ten. The profile says which class each group was in, so the spec has them there:
/// all ten groups are made, every file a name of one, and each class keeps its groups' names.
#[cfg(unix)]
#[test]
fn groups_that_a_packing_rule_cannot_place_are_in_their_classes_and_build_exactly() {
    let scratch = Scratch::new();
    let home = grouped_tree(
        &scratch,
        &[(1, &[6, 2, 2]), (100, &[4, 3, 3]), (5_000, &[3, 3, 2, 2])],
    );
    let shape = walk(&home);
    assert_eq!(
        (
            shape.entries.files,
            shape.hard_links.groups,
            shape.hard_links.names
        ),
        (30, 10, 30)
    );
    assert_eq!(shape.hard_links.group_sizes.buckets.get(2), 8);
    assert_eq!(shape.hard_links.group_sizes.buckets.get(4), 2);

    let spec = spec_from_profile(&shape, &request(31, 1)).expect("a spec the files can hold");
    for seed in 1..=6 {
        let plan = Plan::new(&spec.with_seed(seed)).expect("a plan");
        let by_class = group_names_by_class(&plan);
        assert_eq!(
            by_class.values().map(Vec::len).sum::<usize>(),
            10,
            "seed {seed}: all ten groups"
        );
        assert_eq!(
            by_class.values().flatten().sum::<usize>(),
            30,
            "seed {seed}: every file is a name of a group"
        );
        // Each class has the groups of its histogram and the names it had: ten.
        for (file_class, counts) in &by_class {
            assert_eq!(
                counts.iter().sum::<usize>(),
                10,
                "seed {seed}: {file_class}"
            );
        }
        assert_eq!(by_class[&1].len(), 3, "seed {seed}");
        assert_eq!(by_class[&64].len(), 3, "seed {seed}");
        assert_eq!(by_class[&4096].len(), 4, "seed {seed}");
    }
}

/// The profile of a tree says in which class of file size each group lay, `file_sizes` counts
/// every name, and so the names of a class's groups are among the files of that class. A file
/// with one name is no group.
#[cfg(unix)]
#[test]
fn a_profile_says_in_which_class_of_file_size_each_group_lay() {
    let scratch = Scratch::new();
    // Files of 100 bytes with 3 names and with 1; files of 5,000 bytes with 2, 2, and 5 names;
    // and a file of 7 bytes with one name.
    let home = grouped_tree(&scratch, &[(100, &[3, 1]), (5_000, &[2, 2, 5]), (7, &[1])]);
    let shape = walk(&home);
    shape.check().expect("a consistent profile");

    let histogram = |count, total, max, buckets: &[(u64, u64)]| ShapeHistogram {
        count,
        total,
        max,
        buckets: buckets.iter().copied().collect(),
    };
    assert_eq!(
        shape.hard_links.group_sizes_by_file_size,
        [
            (64, histogram(1, 3, 3, &[(2, 1)])),
            (4096, histogram(3, 9, 5, &[(2, 2), (4, 1)])),
        ]
        .into_iter()
        .collect(),
        "a group in the class of the size of its file, and none for a file with one name"
    );
    assert_eq!(
        shape.hard_links.group_sizes,
        histogram(4, 12, 5, &[(2, 3), (4, 1)]),
        "the classes add up to the groups"
    );
    // Every name is a file of its class: 3 names and the file with one name, 9 names, and 1.
    assert_eq!(shape.file_sizes.buckets.get(64), 4);
    assert_eq!(shape.file_sizes.buckets.get(4096), 9);
    assert_eq!(shape.file_sizes.buckets.get(4), 1);
}
