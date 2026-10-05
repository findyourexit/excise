//! Resolving the path of a cache root: where a path leads, and how long the pathname the system
//! works on gets on the way. Every test makes a link, so they run on Unix only.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use super::support::{LongWay, Scratch, link_to, long_way_round};
use crate::fixture::resolve::{Resolved, resolve};

/// `path` resolved, or the panic that says why not.
fn resolved(path: &Path) -> Resolved {
    resolve(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// How many bytes the path is.
fn bytes(path: &Path) -> usize {
    path.as_os_str().len()
}

/// A path with no link in it resolves to itself, whether it exists or not, and the system works on
/// no pathname longer than the one that was written.
#[test]
fn a_path_with_no_link_resolves_to_itself() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    fs::create_dir_all(base.join("a").join("b")).expect("mkdir");
    for path in [
        base.clone(),
        base.join("a"),
        base.join("a").join("b"),
        base.join("a").join("b").join("not").join("yet"),
        base.join("not"),
    ] {
        let resolution = resolved(&path);
        assert_eq!(resolution.path, path);
        assert_eq!(resolution.longest, bytes(&path), "{}", path.display());
    }

    let root = resolved(Path::new("/"));
    assert_eq!(root.path, Path::new("/"), "the root of the file system");
    assert_eq!(root.longest, 1);
}

/// A link leads where the system leads it, however it is spelled: absolute, relative, through
/// `..`, to another link, with separators and dots in its content, and whether the path ends at
/// it or goes on below. `fs::canonicalize` is the reference.
#[test]
fn a_link_leads_where_the_system_leads_it() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let real = base.join("real");
    fs::create_dir_all(real.join("sub")).expect("mkdir");
    link_to(&real.join("sub").join("back"), Path::new("../.."));
    link_to(&base.join("abs"), &real);
    link_to(&base.join("rel"), Path::new("real/sub"));
    link_to(&base.join("up"), Path::new("real/sub/.."));
    link_to(&base.join("one"), Path::new("two"));
    link_to(&base.join("two"), &base.join("abs").join("sub"));
    link_to(&base.join("dot"), Path::new("."));
    link_to(
        &base.join("slash"),
        Path::new(&format!("{}/", real.display())),
    );
    link_to(&base.join("messy"), Path::new("./real//sub/./"));

    for below in [
        "abs",
        "abs/sub",
        "rel",
        "rel/..",
        "up",
        "up/sub",
        "one",
        "one/..",
        "two/..",
        "real/sub/back",
        "real/sub/back/real/sub",
        "dot/dot/real",
        "slash",
        "slash/sub",
        "messy",
        "messy/..",
        "abs/../rel",
    ] {
        let path = base.join(below);
        assert_eq!(
            resolved(&path).path,
            fs::canonicalize(&path).expect("the system resolves it too"),
            "{below}"
        );
    }

    // Names that are not there follow where the link leads, as they were written.
    for (link, tail) in [
        ("abs", "not/yet"),
        ("rel", "not"),
        ("one", "not/yet/either"),
        ("messy", "not"),
    ] {
        assert_eq!(
            resolved(&base.join(link).join(tail)).path,
            fs::canonicalize(base.join(link))
                .expect("the link resolves")
                .join(tail),
            "{link}/{tail}"
        );
    }
}

/// A link that leads to a name that is not there cannot be resolved, whether the path ends at it
/// or goes on below it. The name of the link exists, so the cache could not make it, and nothing
/// can be made below it. A name that is not there and that no link leads to is no error, and a
/// link whose target appears resolves.
#[test]
fn a_link_that_leads_nowhere_cannot_be_resolved() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    fs::create_dir(base.join("real")).expect("mkdir");
    link_to(&base.join("relative"), Path::new("missing"));
    link_to(&base.join("absolute"), &base.join("missing"));
    link_to(&base.join("deep"), Path::new("real/missing/deeper"));
    link_to(&base.join("chain"), Path::new("relative"));
    let links = ["relative", "absolute", "deep", "chain"];

    for link in links {
        for below in ["", "cache", "not/yet"] {
            let path = if below.is_empty() {
                base.join(link)
            } else {
                base.join(link).join(below)
            };
            let error = resolve(&path).expect_err(&format!("{} leads nowhere", path.display()));
            assert_eq!(error.kind(), io::ErrorKind::NotFound, "{}", path.display());
        }
    }
    assert!(
        resolve(&base.join("missing")).is_ok(),
        "a name that is not there, with no link leading to it, is no error"
    );

    fs::create_dir(base.join("missing")).expect("mkdir");
    fs::create_dir_all(base.join("real/missing/deeper")).expect("mkdir");
    for link in links {
        assert!(
            resolve(&base.join(link).join("cache")).is_ok(),
            "{link}, now that what it leads to is there"
        );
    }
}

/// Links that loop cannot be resolved, and neither can too many in a row: a chain of 8, as many as
/// POSIX says a system must follow, leads to its end, and a chain of 41, more than any system
/// follows, does not.
#[test]
fn links_that_loop_or_are_too_many_cannot_be_resolved() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    link_to(&base.join("a"), Path::new("b"));
    link_to(&base.join("b"), Path::new("a"));
    link_to(&base.join("itself"), Path::new("itself"));
    for link in ["a", "b", "itself"] {
        assert!(resolve(&base.join(link).join("cache")).is_err(), "{link}");
    }

    fs::create_dir(base.join("end")).expect("mkdir");
    for (prefix, count) in [("few", 8), ("many", 41)] {
        for index in 0..count {
            let next = if index + 1 == count {
                "end".to_owned()
            } else {
                format!("{prefix}{}", index + 1)
            };
            link_to(&base.join(format!("{prefix}{index}")), Path::new(&next));
        }
    }
    assert_eq!(resolved(&base.join("few0")).path, base.join("end"));
    assert!(resolve(&base.join("many0")).is_err());
}

/// The system works on the content of a link and what is left of the path after it, and that can
/// be longer than the path as it was written and as it resolves. A short link to a long path that
/// ends in a link back to a short directory is the case: the path below it is short both ways.
#[test]
fn the_pathname_a_link_makes_the_system_work_on_counts_though_it_is_longer_than_both_ends() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let LongWay { near, back, start } = long_way_round(&base, 700);

    let through = start.join("cache");
    let resolution = resolved(&through);
    assert_eq!(resolution.path, near.join("cache"));
    assert!(
        bytes(&through) + 500 < resolution.longest
            && bytes(&resolution.path) + 500 < resolution.longest,
        "the path is short as it is written and as it resolves"
    );
    assert_eq!(
        resolution.longest,
        bytes(&back) + "/cache".len(),
        "the content of `start`, and what is left of the path after it"
    );

    // What is left after the link is every name written after it, the ones that are not there
    // yet included.
    assert_eq!(
        resolved(&start.join("cache/below/it")).longest,
        bytes(&back) + "/cache/below/it".len()
    );

    // A link to a link counts the longest of its expansions, which is the second.
    link_to(&base.join("outer"), Path::new("start"));
    assert_eq!(
        resolved(&base.join("outer").join("cache")).longest,
        bytes(&back) + "/cache".len()
    );
}

/// The content of a link counts as it is written, whether it is absolute or relative. A relative
/// one counts without the folder that holds the link, which the system does not put in the
/// pathname it works on, and one that goes down a long way and comes back up with `..` counts in
/// full, though the path resolves short.
#[test]
fn the_content_of_a_link_counts_as_it_is_written() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let LongWay { near, back, .. } = long_way_round(&base, 700);
    let deep = back.parent().expect("a deep directory");

    let relative = back.strip_prefix(&base).expect("below the base");
    link_to(&base.join("relative"), relative);
    let resolution = resolved(&base.join("relative").join("cache"));
    assert_eq!(resolution.path, near.join("cache"));
    assert_eq!(resolution.longest, bytes(relative) + "/cache".len());

    let levels = deep
        .strip_prefix(&base)
        .expect("below the base")
        .components()
        .count();
    let mut climb = deep.to_path_buf();
    for _ in 0..levels {
        climb.push("..");
    }
    climb.push("near");
    link_to(&base.join("up"), &climb);
    let resolution = resolved(&base.join("up").join("cache"));
    assert_eq!(resolution.path, near.join("cache"));
    assert_eq!(resolution.longest, bytes(&climb) + "/cache".len());
}

/// What the system works on is the text it was given, separators and all: a link whose content
/// ends in two separators makes the pathname after the next link two bytes longer than the names
/// alone, and the count includes them.
#[test]
fn separators_in_the_content_of_a_link_count() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let LongWay { back, .. } = long_way_round(&base, 700);

    // `first` leads to `start`, with two separators after it, and `start` leads to `back`. The
    // pathname made at `first` is `start//` and `/cache`; the one made at `start` is the content
    // of `start`, the two separators that follow `start` in the content of `first`, and `/cache`.
    link_to(&base.join("first"), Path::new("start//"));
    assert_eq!(
        resolved(&base.join("first").join("cache")).longest,
        bytes(&back) + "//".len() + "/cache".len()
    );
}

/// The path is taken as it is spelled, which is how the system is handed it: repeated separators
/// and `.` names make it longer, and the path it resolves to has none of them. A relative path is
/// the current directory and the text written after it, and an empty one cannot be made absolute.
#[test]
fn a_path_counts_as_it_is_spelled() {
    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    fs::create_dir(base.join("real")).expect("a folder");
    let plain = base.join("real").join("cache");
    let spelled = |tail: &str| PathBuf::from(format!("{}{tail}", base.display()));
    for (written, extra) in [
        (spelled("/real/cache"), 0),
        (spelled("//real/cache"), 1),
        (spelled("/./real/cache"), 2),
        (spelled("/real//cache"), 1),
        (spelled("/real/./cache"), 2),
        (spelled("/real/cache/"), 1),
        (spelled("/real/cache/."), 2),
        (spelled("///real///./cache//"), 8),
    ] {
        assert_eq!(
            bytes(&written),
            bytes(&plain) + extra,
            "{}",
            written.display()
        );
        let resolution = resolved(&written);
        assert_eq!(resolution.path, plain, "{}", written.display());
        assert_eq!(resolution.longest, bytes(&written), "{}", written.display());
    }

    let current = std::env::current_dir().expect("a current directory");
    let relative = Path::new("./a-root//below/./the-current-directory");
    let resolution = resolved(relative);
    assert_eq!(
        resolution.path,
        current.join("a-root/below/the-current-directory")
    );
    assert_eq!(resolution.longest, bytes(&current.join(relative)));

    let error = resolve(Path::new("")).expect_err("an empty path names nothing");
    assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
}

/// Every name that exists has to be a folder, the last one of the path included, once its links
/// are expanded. The system refuses a path that goes on after a file with `ENOTDIR`, with a
/// separator, a `.`, or a `..`, and a cache has to be in a folder. So a root that is a file, a
/// link to one, and a link whose content goes on after one, in each of those ways, cannot be
/// resolved, nor can a root written so, nor a path below one.
#[test]
fn a_name_that_exists_and_is_not_a_folder_cannot_be_resolved() {
    use nix::{sys::stat::Mode, unistd::mkfifo};

    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    fs::write(base.join("file"), b"not a folder").expect("a file");
    fs::create_dir(base.join("real")).expect("a folder");
    mkfifo(&base.join("fifo"), Mode::S_IRUSR | Mode::S_IWUSR).expect("a FIFO");
    link_to(&base.join("to-file"), Path::new("file"));
    link_to(&base.join("slash"), Path::new("file/"));
    link_to(&base.join("dot"), Path::new("file/."));
    link_to(&base.join("up"), Path::new("file/../real"));
    link_to(&base.join("chain"), Path::new("to-file"));
    let spelled = |tail: &str| PathBuf::from(format!("{}/{tail}", base.display()));

    for path in [
        base.join("file"),
        base.join("fifo"),
        base.join("to-file"),
        base.join("slash"),
        base.join("dot"),
        base.join("up"),
        base.join("chain"),
        spelled("file/"),
        spelled("file/."),
        spelled("file/.."),
        spelled("file/../real"),
        spelled("to-file/"),
        spelled("fifo/"),
        base.join("slash").join("cache"),
        base.join("file").join("cache"),
    ] {
        let error = resolve(&path).expect_err(&format!("{} is not a folder", path.display()));
        assert_eq!(
            error.kind(),
            io::ErrorKind::NotADirectory,
            "{}",
            path.display()
        );
    }

    // A folder is another matter, however the path goes on after it.
    link_to(&base.join("folder-slash"), Path::new("real/"));
    link_to(&base.join("folder-dot"), Path::new("real/."));
    link_to(&base.join("folder-up"), Path::new("real/../real"));
    for path in [
        base.join("real"),
        spelled("real/"),
        spelled("real/."),
        spelled("real/.."),
        spelled("real/../real"),
        base.join("folder-slash"),
        base.join("folder-dot"),
        base.join("folder-up"),
    ] {
        assert_eq!(
            resolved(&path).path,
            fs::canonicalize(&path).expect("the system resolves it too"),
            "{}",
            path.display()
        );
    }
}

/// The count rests on what macOS does: it refuses a pathname made of the content of a link and what
/// is left of the path when it is longer than 1,023 bytes, and takes one of 1,023. Around the
/// limit, what the system refuses is what the resolver counts as too long.
#[cfg(target_os = "macos")]
#[test]
fn what_is_counted_is_what_macos_refuses_past_1023_bytes() {
    use crate::fixture::spec::MAX_PORTABLE_PATH_BYTES;

    let scratch = Scratch::new();
    let base = scratch.resolved_path();
    let LongWay { near, back, start } = long_way_round(&base, 700);
    let limit = usize::try_from(MAX_PORTABLE_PATH_BYTES).expect("a small number");

    let first = "n".repeat(200);
    for formed in limit - 1..=limit + 2 {
        // The pathname after `start`: the 705 bytes of its content, a separator, `first`, a
        // separator, and a second name that is as long as it takes.
        let second = "n".repeat(formed - bytes(&back) - 1 - first.len() - 1);
        fs::create_dir_all(near.join(&first).join(&second)).expect("a directory to reach");
        let through = start.join(&first).join(&second);

        assert_eq!(
            resolved(&through).longest,
            formed,
            "what the resolver counts"
        );
        let reached = fs::symlink_metadata(&through);
        if formed <= limit {
            reached.unwrap_or_else(|error| panic!("{formed} bytes are refused: {error}"));
        } else {
            let error = reached.expect_err(&format!("{formed} bytes are taken"));
            assert_eq!(
                error.raw_os_error(),
                Some(rustix::io::Errno::NAMETOOLONG.raw_os_error()),
                "{formed} bytes are refused for another reason: {error}"
            );
        }
    }
}
