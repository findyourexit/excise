//! The live mutators: each operation does what it says, and none can be steered outside the
//! fixture or onto the marker.

use std::{
    fs,
    path::{Path, PathBuf},
};

use super::support::Scratch;
#[cfg(unix)]
use super::support::master;
use crate::{
    fixture::{
        MARKER_FILE_NAME, NodeKind,
        mutate::{APPEAR_BYTES, CHANGE_BYTES, MutateError, REPLACE_BYTES, apply},
    },
    scenario::MutateOp,
};

/// A marked fixture root holding `dir/file.txt` (10 bytes) and `top.txt`, built by hand so these
/// tests do not depend on the generator.
fn small_root(scratch: &Scratch, name: &str) -> PathBuf {
    let root = scratch.join(name);
    fs::create_dir_all(root.join("dir/sub")).expect("mkdir");
    fs::write(root.join(MARKER_FILE_NAME), "marker").expect("marker");
    fs::write(root.join("top.txt"), "0123456789").expect("write");
    fs::write(root.join("dir/file.txt"), "0123456789").expect("write");
    fs::write(root.join("dir/sub/deep.txt"), "x").expect("write");
    root
}

fn size(path: &Path) -> u64 {
    fs::symlink_metadata(path).expect("stat").len()
}

#[test]
fn appear_creates_a_file_and_the_directories_above_it() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");

    let done = apply(&root, MutateOp::Appear, "dir/new.txt").expect("appear");
    assert_eq!(size(&root.join("dir/new.txt")), APPEAR_BYTES);
    assert_eq!(done.before, None);
    assert_eq!(done.after, Some(NodeKind::File));
    assert_eq!(done.size_after, Some(APPEAR_BYTES));

    apply(&root, MutateOp::Appear, "made/on/demand/file.bin")
        .expect("appear below new directories");
    assert_eq!(size(&root.join("made/on/demand/file.bin")), APPEAR_BYTES);
    assert!(root.join("made/on/demand").is_dir());

    // A path that exists, as a file or a directory, is not created over.
    assert!(matches!(
        apply(&root, MutateOp::Appear, "top.txt"),
        Err(MutateError::AlreadyExists { .. })
    ));
    assert!(matches!(
        apply(&root, MutateOp::Appear, "dir/sub"),
        Err(MutateError::AlreadyExists { .. })
    ));
    // A file where a directory must be.
    assert!(matches!(
        apply(&root, MutateOp::Appear, "top.txt/child"),
        Err(MutateError::NotADirectory { .. })
    ));
}

#[test]
fn appear_content_is_a_function_of_the_path() {
    let scratch = Scratch::new();
    let one = small_root(&scratch, "one");
    let two = small_root(&scratch, "two");
    for root in [&one, &two] {
        apply(root, MutateOp::Appear, "dir/new.txt").expect("appear");
        apply(root, MutateOp::Appear, "other.txt").expect("appear");
    }
    assert_eq!(
        fs::read(one.join("dir/new.txt")).expect("read"),
        fs::read(two.join("dir/new.txt")).expect("read")
    );
    assert_ne!(
        fs::read(one.join("dir/new.txt")).expect("read"),
        fs::read(one.join("other.txt")).expect("read"),
        "different paths get different bytes"
    );
}

#[test]
fn change_appends_to_the_same_file() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    let before = fs::read(root.join("dir/file.txt")).expect("read");
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(
        &fs::metadata(root.join("dir/file.txt")).expect("stat"),
    );

    let done = apply(&root, MutateOp::Change, "dir/file.txt").expect("change");
    assert_eq!(done.before, Some(NodeKind::File));
    assert_eq!(done.size_after, Some(10 + CHANGE_BYTES));
    let after = fs::read(root.join("dir/file.txt")).expect("read");
    assert_eq!(after.len() as u64, 10 + CHANGE_BYTES);
    assert!(
        after.starts_with(&before),
        "the original bytes stay, new bytes are appended"
    );
    #[cfg(unix)]
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(
            &fs::metadata(root.join("dir/file.txt")).expect("stat")
        ),
        inode,
        "a change keeps the file"
    );

    assert!(matches!(
        apply(&root, MutateOp::Change, "dir"),
        Err(MutateError::WrongKind {
            found: NodeKind::Directory,
            ..
        })
    ));
    assert!(matches!(
        apply(&root, MutateOp::Change, "missing.txt"),
        Err(MutateError::NotFound { .. })
    ));
    assert!(matches!(
        apply(&root, MutateOp::Change, "absent-dir/file.txt"),
        Err(MutateError::NotFound { .. })
    ));
}

#[test]
fn vanish_removes_a_file_or_a_whole_directory() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    let done = apply(&root, MutateOp::Vanish, "top.txt").expect("vanish a file");
    assert_eq!(done.before, Some(NodeKind::File));
    assert_eq!(done.after, None);
    assert!(!root.join("top.txt").exists());

    apply(&root, MutateOp::Vanish, "dir").expect("vanish a directory and everything in it");
    assert!(!root.join("dir").exists());
    assert!(root.join(MARKER_FILE_NAME).exists());

    assert!(matches!(
        apply(&root, MutateOp::Vanish, "dir"),
        Err(MutateError::NotFound { .. })
    ));
}

#[cfg(unix)]
#[test]
fn vanish_removes_a_link_and_never_what_it_points_to() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    symlink("dir", root.join("link-to-dir")).expect("symlink");
    symlink("top.txt", root.join("link-to-file")).expect("symlink");
    symlink("nowhere", root.join("dangling")).expect("symlink");

    for name in ["link-to-dir", "link-to-file", "dangling"] {
        let done = apply(&root, MutateOp::Vanish, name).expect("vanish a link");
        assert_eq!(done.before, Some(NodeKind::Symlink));
        assert!(fs::symlink_metadata(root.join(name)).is_err());
    }
    assert!(
        root.join("dir/file.txt").exists(),
        "the directory the link named is untouched"
    );
    assert!(
        root.join("top.txt").exists(),
        "the file the link named is untouched"
    );
}

#[test]
fn replace_gives_a_file_a_new_identity_under_the_same_name() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(
        &fs::metadata(root.join("dir/file.txt")).expect("stat"),
    );

    let done = apply(&root, MutateOp::Replace, "dir/file.txt").expect("replace a file");
    assert_eq!(done.before, Some(NodeKind::File));
    assert_eq!(done.after, Some(NodeKind::File));
    assert_eq!(done.size_after, Some(REPLACE_BYTES));
    assert_eq!(size(&root.join("dir/file.txt")), REPLACE_BYTES);
    #[cfg(unix)]
    assert_ne!(
        std::os::unix::fs::MetadataExt::ino(
            &fs::metadata(root.join("dir/file.txt")).expect("stat")
        ),
        inode,
        "a replaced file is a different file"
    );
    // No temporary is left in the directory.
    let names: Vec<String> = fs::read_dir(root.join("dir"))
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert_eq!(
        names
            .iter()
            .filter(|name| name.starts_with(".excise-harness-"))
            .count(),
        0,
        "{names:?}"
    );
}

#[test]
fn replace_gives_a_directory_a_new_empty_one_under_the_same_name() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    let done = apply(&root, MutateOp::Replace, "dir").expect("replace a directory");
    assert_eq!(done.before, Some(NodeKind::Directory));
    assert_eq!(done.after, Some(NodeKind::Directory));
    assert!(root.join("dir").is_dir());
    assert_eq!(
        fs::read_dir(root.join("dir")).expect("list").count(),
        0,
        "the new directory is empty"
    );
}

#[cfg(unix)]
#[test]
fn replace_refuses_a_link_and_change_refuses_it_too() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    std::os::unix::fs::symlink("top.txt", root.join("link")).expect("symlink");
    assert!(matches!(
        apply(&root, MutateOp::Replace, "link"),
        Err(MutateError::WrongKind {
            found: NodeKind::Symlink,
            ..
        })
    ));
    assert!(matches!(
        apply(&root, MutateOp::Change, "link"),
        Err(MutateError::WrongKind {
            found: NodeKind::Symlink,
            ..
        })
    ));
    assert_eq!(
        size(&root.join("top.txt")),
        10,
        "the link's target was not written through"
    );
}

#[test]
fn a_path_that_could_leave_the_fixture_is_rejected_before_anything_happens() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    fs::write(scratch.join("outside.txt"), "precious").expect("write");

    for path in [
        "../outside.txt",
        "dir/../../outside.txt",
        "/etc/passwd",
        "/",
        "\\windows\\system32",
        "C:\\Windows",
        "C:x",
        "",
        "dir//file.txt",
        "dir/",
        "./top.txt",
        "dir/./file.txt",
        "file.txt:stream",
        "NUL",
        "trailing.",
    ] {
        for op in MutateOp::ALL {
            match apply(&root, *op, path) {
                Err(MutateError::InvalidPath { .. }) => {}
                other => panic!("`{path}` with `{op}` should be an invalid path, got {other:?}"),
            }
        }
    }
    assert_eq!(
        fs::read(scratch.join("outside.txt")).expect("read"),
        b"precious"
    );
    assert_eq!(size(&root.join("top.txt")), 10);
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_in_the_path_is_never_followed() {
    use std::os::unix::fs::symlink;

    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    let outside = scratch.join("outside");
    fs::create_dir(&outside).expect("mkdir");
    fs::write(outside.join("victim.txt"), "precious").expect("write");
    symlink(&outside, root.join("escape")).expect("symlink");
    symlink("dir", root.join("inside")).expect("symlink");

    for op in MutateOp::ALL {
        for path in [
            "escape/victim.txt",
            "escape/new.txt",
            "inside/file.txt",
            "inside/sub/deep.txt",
        ] {
            match apply(&root, *op, path) {
                Err(MutateError::SymlinkTraversal { component, .. }) => {
                    assert!(
                        component == "escape" || component == "inside",
                        "{component}"
                    );
                }
                other => panic!("`{path}` with `{op}` must not traverse a link, got {other:?}"),
            }
        }
    }
    assert_eq!(
        fs::read(outside.join("victim.txt")).expect("read"),
        b"precious"
    );
    assert!(
        !outside.join("new.txt").exists(),
        "appear did not create a file through the link"
    );
    assert_eq!(size(&root.join("dir/file.txt")), 10);
}

#[test]
fn the_marker_and_unmarked_roots_are_protected() {
    let scratch = Scratch::new();
    let root = small_root(&scratch, "root");
    for op in MutateOp::ALL {
        assert!(
            matches!(
                apply(&root, *op, MARKER_FILE_NAME),
                Err(MutateError::Protected { .. })
            ),
            "{op} on the marker"
        );
    }
    assert_eq!(
        fs::read(root.join(MARKER_FILE_NAME)).expect("read"),
        b"marker"
    );

    // A directory without the marker is not a fixture, whatever it holds.
    let unmarked = scratch.join("unmarked");
    fs::create_dir_all(&unmarked).expect("mkdir");
    fs::write(unmarked.join("precious.txt"), "keep").expect("write");
    for op in MutateOp::ALL {
        assert!(
            matches!(
                apply(&unmarked, *op, "precious.txt"),
                Err(MutateError::Unowned(_))
            ),
            "{op} in an unmarked root"
        );
    }
    assert_eq!(
        fs::read(unmarked.join("precious.txt")).expect("read"),
        b"keep"
    );
    assert!(matches!(
        apply(&scratch.join("absent"), MutateOp::Appear, "x"),
        Err(MutateError::Unowned(_))
    ));
}

#[cfg(unix)]
#[test]
fn vanish_and_replace_can_remove_a_generated_tree_that_has_unreadable_parts() {
    let scratch = Scratch::new();
    let master = master(&scratch, "hostile-small");
    let locked = master.root.join("hostile/unreadable");
    assert!(locked.exists());

    let done = apply(&master.root, MutateOp::Replace, "hostile/unreadable").expect("replace");
    assert_eq!(done.after, Some(NodeKind::Directory));
    assert_eq!(fs::read_dir(&locked).expect("list").count(), 0);

    apply(&master.root, MutateOp::Vanish, "hostile/long").expect("vanish");
    assert!(!master.root.join("hostile/long").exists());
}
