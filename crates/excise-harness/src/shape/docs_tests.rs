//! The development guide says what `excise-shape profile` asks of the file system as it is. The
//! walk asks for the metadata of each entry with `lstat` and nothing else about it: it never asks
//! what a symbolic link points at, not even whether the target exists, because a call that follows
//! a link leaves the tree. A guide that promises more than that, or that still tells of a `stat` of
//! each link, is wrong about what the walk does, and `excise-shape help profile` and the harness
//! README say what it does. The same goes for its memory: the guide and the README say what it
//! grows with, and never that it does not grow with the size of the tree.

/// The development guide, as it is in this checkout.
const DEVELOPMENT_GUIDE: &str = include_str!("../../../../docs/development.md");

/// The harness README, as it is in this checkout.
const HARNESS_README: &str = include_str!("../../README.md");

/// The words of the paragraph of `text` that starts with `start`, joined by single spaces, so that
/// a promise is not broken by where a line wraps.
fn paragraph(text: &str, start: &str) -> String {
    let words: Vec<&str> = text
        .lines()
        .skip_while(|line| !line.starts_with(start))
        .take_while(|line| !line.trim().is_empty())
        .flat_map(str::split_whitespace)
        .collect();
    assert!(
        !words.is_empty(),
        "no paragraph of the guide starts with `{start}`"
    );
    words.join(" ")
}

#[test]
fn the_guide_says_the_walk_asks_lstat_only_and_never_looks_behind_a_link() {
    let walk = paragraph(
        DEVELOPMENT_GUIDE,
        "`excise-shape`, the binary of the `excise-harness` crate",
    );

    for promise in [
        "(`lstat` only: it asks nothing of what a symbolic link points at, not even whether the \
         target exists, writes nothing, follows no link below the root",
        "Its memory grows with three things of the tree and nothing else: the names of the folder \
         it is listing, the subfolders still to visit in each folder on the path to it (in a \
         comb-shaped tree, a chain of folders that each hold many subfolders, they can approach \
         the number of folders in the tree), and the identities of the files that have more than \
         one name; it spills nothing to disk",
    ] {
        assert!(
            walk.contains(promise),
            "the guide says `{promise}` of the walk of `excise-shape profile`: {walk}"
        );
    }
    // What it used to say: the walk made one `stat` of each symbolic link, to tell whether its
    // target exists, and the profile counted the links that dangle; and its memory was said never
    // to be the size of the tree, which a comb of folders, or a tree of hard links, makes it.
    for retracted in [
        "apart from one `stat`",
        "to tell whether its target exists",
        "dangle",
        "never the size of the tree",
        "never grows with the size of the tree",
    ] {
        assert!(
            !walk.contains(retracted),
            "the guide no longer says `{retracted}`: {walk}"
        );
    }
}

#[test]
fn the_readme_says_what_the_memory_of_the_walk_grows_with() {
    let streams = paragraph(HARNESS_README, "- **It streams, with a few handles.**");

    for promise in [
        "its memory grows with those three and with nothing else of the tree",
        "the walk spills nothing to disk, so a tree for which any of them does not fit in what \
         follows cannot be profiled",
        "This grows with the widest folder, which is what costs",
        "so this grows with the subfolders that wait along a path, and not with the widest folder's \
         alone",
        "in a comb, a chain of folders that each hold many subfolders besides the next one of the \
         chain, the folders waiting along the chain can approach the number of folders in the \
         tree",
        "This grows with the number of such files, up to every file of the tree",
    ] {
        assert!(
            streams.contains(promise),
            "the README says `{promise}` of the walk of `excise-shape profile`: {streams}"
        );
    }
    for retracted in [
        "never grows with the size of the tree",
        "never the size of the tree",
    ] {
        assert!(
            !streams.contains(retracted),
            "the README no longer says `{retracted}`: {streams}"
        );
    }
}
