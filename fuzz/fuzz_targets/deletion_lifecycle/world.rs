//! The fixture the runtime is pointed at, the snapshots the model compares, and the mutations
//! the script applies to the live tree.
//!
//! ```text
//! <base>/root/      the scan root: folders, files, an empty folder, symbolic links, a
//!                   hard-linked pair, and a hard link to a file outside the root
//! <base>/outside/   sentinels beside the scan root, never mutated by the script
//! <base>/store/     where the runtime keeps its scan store; not part of the world
//! <base>/cwd/       the working directory of the run, where a stray export would land
//! ```
//!
//! One variant of the fixture also holds a folder and a file in `alpha` that carry names the
//! mutations give their new entries, so that a rename, or a replacement, can land on an entry
//! that the planner reviews.
//!
//! # Platforms
//!
//! Every mutation runs wherever the file system lets it, with two exceptions. Where the platform
//! does not create symbolic links (anything but Unix), the fixture holds none, and the mutation
//! that replaces an entry with one leaves the tree as it is: it does not remove the entry first.
//! Where the number of names an entry has is not read (also anything but Unix), the mutation that
//! removes a hard link finds no entry to act on. Only the Unix paths have run.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use file_id::FileId;

use crate::script::{MutationKind, Op, Shape, select};

/// The directories of one run.
#[derive(Clone, Debug)]
pub struct World {
    /// The private directory that holds all of the others.
    pub base: PathBuf,
    pub root: PathBuf,
    pub outside: PathBuf,
    pub store: PathBuf,
    pub cwd: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    File,
    Folder,
    Symlink,
    Other,
}

/// What the model remembers of one name in the tree.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    pub kind: Kind,
    pub id: FileId,
    /// A file's length. Nothing outside a confirmed target is ever written.
    pub len: u64,
    /// Where a symbolic link points.
    pub link: Option<PathBuf>,
    /// Which entry the name holds, as the harness numbers them ([`Generations`]). A file system
    /// may hand a replacement the identity of the entry it replaced; the generation still tells
    /// the two apart.
    pub generation: u64,
}

/// Every name in the scan root and in the sentinel folder, by absolute path.
pub type Snapshot = BTreeMap<PathBuf, Entry>;

/// Numbers the entries of the tree, which the file system's own identity cannot do: a file system
/// may give a replacement the number of the entry it replaced (ext4 and tmpfs reuse inode numbers
/// at once), and a reused number must not make a replacement look like the entry that was
/// reviewed.
///
/// A name gets a fresh generation when a snapshot first sees an entry there, and again when the
/// harness says that it replaced the entry or moved another into the name
/// ([`Generations::touch`]), though a snapshot sees an entry there both before and after. A name
/// that holds nothing forgets its generation.
#[derive(Default)]
pub struct Generations {
    next: u64,
    stamps: BTreeMap<PathBuf, u64>,
}

impl Generations {
    /// What `path` holds is no longer the entry it held: the harness replaced it, or moved another
    /// entry in.
    pub fn touch(&mut self, path: &Path) {
        self.stamps.remove(path);
    }

    fn stamp(&mut self, path: &Path) -> u64 {
        if let Some(stamp) = self.stamps.get(path) {
            return *stamp;
        }
        self.next += 1;
        self.stamps.insert(path.to_path_buf(), self.next);
        self.next
    }

    fn forget_vanished(&mut self, seen: &Snapshot) {
        self.stamps.retain(|path, _| seen.contains_key(path));
    }
}

#[cfg(unix)]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the other platforms read the identity through a handle, which can fail"
)]
fn file_id_of(_path: &Path, metadata: &fs::Metadata) -> io::Result<FileId> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(FileId::new_inode(metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_id_of(path: &Path, metadata: &fs::Metadata) -> io::Result<FileId> {
    excise::fuzz::native_path::identity_for(path, metadata)?
        .map(|identity| identity.file_id)
        .ok_or_else(|| io::Error::other("the entry has no identity"))
}

/// Takes the snapshot of the whole world. It never follows a symbolic link.
pub fn snapshot(world: &World, generations: &mut Generations) -> io::Result<Snapshot> {
    let mut entries = Snapshot::new();
    walk(&world.root, generations, &mut entries)?;
    walk(&world.outside, generations, &mut entries)?;
    generations.forget_vanished(&entries);
    Ok(entries)
}

fn walk(path: &Path, generations: &mut Generations, entries: &mut Snapshot) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    let kind = if file_type.is_dir() {
        Kind::Folder
    } else if file_type.is_symlink() {
        Kind::Symlink
    } else if file_type.is_file() {
        Kind::File
    } else {
        Kind::Other
    };
    entries.insert(
        path.to_path_buf(),
        Entry {
            kind,
            id: file_id_of(path, &metadata)?,
            len: if kind == Kind::File {
                metadata.len()
            } else {
                0
            },
            link: if kind == Kind::Symlink {
                Some(fs::read_link(path)?)
            } else {
                None
            },
            generation: generations.stamp(path),
        },
    );
    if kind == Kind::Folder {
        for child in fs::read_dir(path)? {
            walk(&child?.path(), generations, entries)?;
        }
    }
    Ok(())
}

// --- the fixture -------------------------------------------------------------------------------

const ALPHA: usize = 0;
const BETA: usize = 1;
const EMPTY: usize = 2;
const BIG: usize = 3;
const SMALL: usize = 4;
const ZERO: usize = 5;
const A1: usize = 6;
const A2: usize = 7;
const DEEP: usize = 8;
const D1: usize = 9;
const B1: usize = 10;
const B2: usize = 11;
#[cfg(unix)]
const LINK_OUT: usize = 12;
#[cfg(unix)]
const LINK_FILE: usize = 13;
#[cfg(unix)]
const LINK_DANGLING: usize = 14;
#[cfg(unix)]
const LINK_IN: usize = 15;
const HARD_A: usize = 16;
const HARD_B: usize = 17;
const HARD_OUT: usize = 18;

const NAMES: [&str; 19] = [
    "alpha",
    "beta",
    "empty",
    "big.bin",
    "small.txt",
    "zero.log",
    "a1.txt",
    "a2.log",
    "deep",
    "d1.txt",
    "b1.bin",
    "b2.txt",
    "link-out",
    "link-file",
    "link-dangling",
    "link-in",
    "hard-a",
    "hard-b",
    "hard-out",
];

/// The name of fixture entry `index` in naming style `style`. The styles sort differently:
/// plain lowercase, mixed case, numbers that sort one way as text and another as numbers, and
/// names that start with punctuation or hold spaces.
fn name(style: u8, index: usize) -> String {
    let base = NAMES[index];
    match style {
        1 if index % 2 == 1 => {
            let mut characters = base.chars();
            characters.next().map_or_else(String::new, |first| {
                first.to_ascii_uppercase().to_string() + characters.as_str()
            })
        }
        0 | 1 => base.to_owned(),
        2 => format!("{}-{base}", NAMES.len() - index),
        _ => format!(
            "{}{}",
            ['.', '-', '_', '~', '+', '@'][index % 6],
            base.replace('-', " ")
        ),
    }
}

/// The length of fixture file `slot` in size style `style`.
fn size(style: u8, slot: usize) -> usize {
    match style {
        0 => 4096,
        1 => [1, 10, 100, 1_000, 10_000, 70_000, 5, 50][slot % 8],
        2 => [200_000, 3_000, 3_000, 512, 512, 4_096, 1, 0][slot % 8],
        _ => [0, 0, 0, 1, 0, 0, 100_000, 0][slot % 8],
    }
}

fn write_file(path: &Path, len: usize) -> io::Result<()> {
    fs::write(path, vec![b'x'; len])
}

impl World {
    /// Lays out `base` and builds the fixture for `shape` in it.
    pub fn build(base: &Path, shape: Shape) -> io::Result<Self> {
        let world = Self {
            base: base.to_path_buf(),
            root: base.join("root"),
            outside: base.join("outside"),
            store: base.join("store"),
            cwd: base.join("cwd"),
        };
        for directory in [&world.root, &world.outside, &world.store, &world.cwd] {
            fs::create_dir(directory)?;
        }

        let style = shape.names;
        let at = |index: usize| name(style, index);
        let sized = |slot: usize| size(shape.sizes, slot);

        // Sentinels: nothing the script ever touches, and nothing a confirmed target contains.
        write_file(&world.outside.join("sentinel-file"), 64)?;
        fs::create_dir(world.outside.join("sentinel-dir"))?;
        write_file(&world.outside.join("sentinel-dir").join("nested-file"), 32)?;
        write_file(&world.outside.join("sentinel-hard"), 2_000)?;

        let alpha = world.root.join(at(ALPHA));
        let beta = world.root.join(at(BETA));
        let deep = alpha.join(at(DEEP));
        for directory in [&alpha, &beta, &deep, &world.root.join(at(EMPTY))] {
            fs::create_dir(directory)?;
        }
        write_file(&world.root.join(at(BIG)), sized(0))?;
        write_file(&world.root.join(at(SMALL)), sized(1))?;
        write_file(&world.root.join(at(ZERO)), sized(2))?;
        write_file(&alpha.join(at(A1)), sized(3))?;
        write_file(&alpha.join(at(A2)), sized(4))?;
        write_file(&deep.join(at(D1)), sized(5))?;
        write_file(&beta.join(at(B1)), sized(6))?;
        write_file(&beta.join(at(B2)), sized(7))?;

        // A hard-linked pair inside the root, and a hard link to a sentinel.
        write_file(&alpha.join(at(HARD_A)), 3_000)?;
        fs::hard_link(alpha.join(at(HARD_A)), beta.join(at(HARD_B)))?;
        fs::hard_link(world.outside.join("sentinel-hard"), beta.join(at(HARD_OUT)))?;

        // Links that point out of the root, to nothing, and into it. None is ever followed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            symlink(
                world.outside.join("sentinel-dir"),
                world.root.join(at(LINK_OUT)),
            )?;
            symlink(
                world.outside.join("sentinel-file"),
                world.root.join(at(LINK_FILE)),
            )?;
            symlink("nowhere-at-all", world.root.join(at(LINK_DANGLING)))?;
            symlink(at(ALPHA), world.root.join(at(LINK_IN)))?;
        }

        match shape.extra {
            1 => {
                let deeper = deep.join("deeper");
                fs::create_dir(&deeper)?;
                write_file(&deeper.join("d2.txt"), sized(1))?;
                // Entries named as the mutations name theirs ([`FRESH`]): a mutation that lands
                // on one, or acts on one under its own name, acts on an entry the planner reviews.
                fs::create_dir(alpha.join(FRESH[0]))?;
                write_file(&alpha.join(FRESH[0]).join("inner.txt"), sized(2))?;
                write_file(&alpha.join(FRESH[6]), sized(3))?;
            }
            2 => {
                for slot in 0..3 {
                    write_file(&beta.join(format!("x{slot}.txt")), sized(slot + 2))?;
                }
            }
            3 => {
                fs::create_dir(world.root.join("empty2"))?;
                fs::create_dir(world.root.join("empty2").join("inner"))?;
                write_file(&world.root.join("tail.txt"), sized(4))?;
            }
            _ => {}
        }
        Ok(world)
    }

    /// Whether `path` is one of the sentinels beside the scan root.
    pub fn is_sentinel(&self, path: &Path) -> bool {
        path.starts_with(&self.outside)
    }
}

// --- mutations ---------------------------------------------------------------------------------

/// The names a mutation gives new entries.
const FRESH: [&str; 8] = [
    "late", "Late", "7-late", ".late", "late.txt", "late.log", "zz", "A",
];
/// The lengths a mutation gives new files.
const LENGTHS: [usize; 4] = [0, 1, 700, 5_000];
/// The bytes a mutation appends to a file.
const GROWTH: [usize; 3] = [1, 100, 4_096];

/// A name that does not exist yet in `directory`, from `FRESH`.
fn fresh(directory: &Path, selector: u8) -> PathBuf {
    let base = FRESH[select(selector, FRESH.len())];
    let mut candidate = directory.join(base);
    for suffix in 1..=8 {
        if fs::symlink_metadata(&candidate).is_err() {
            break;
        }
        candidate = directory.join(format!("{base}-{suffix}"));
    }
    candidate
}

/// Picks element `selector` of `items`, wrapping, or `None` when there is none.
fn pick<T>(items: &[T], selector: u8) -> Option<&T> {
    if items.is_empty() {
        None
    } else {
        items.get(select(selector, items.len()))
    }
}

/// What the script can act on, in a fixed order.
struct Candidates {
    /// Every folder below and including the scan root.
    folders: Vec<PathBuf>,
    /// Every name below the scan root.
    entries: Vec<(PathBuf, Kind)>,
    /// Every name that is not a folder.
    non_folders: Vec<PathBuf>,
    /// Every regular file.
    files: Vec<PathBuf>,
    /// Every regular file with more than one name.
    linked: Vec<PathBuf>,
}

impl Candidates {
    fn of(world: &World, snapshot: &Snapshot) -> Self {
        let mut found = Self {
            folders: Vec::new(),
            entries: Vec::new(),
            non_folders: Vec::new(),
            files: Vec::new(),
            linked: Vec::new(),
        };
        for (path, entry) in snapshot {
            if !path.starts_with(&world.root) {
                continue;
            }
            if entry.kind == Kind::Folder {
                found.folders.push(path.clone());
            }
            if path == &world.root {
                continue;
            }
            found.entries.push((path.clone(), entry.kind));
            if entry.kind != Kind::Folder {
                found.non_folders.push(path.clone());
            }
            if entry.kind == Kind::File {
                found.files.push(path.clone());
                if has_other_names(path) {
                    found.linked.push(path.clone());
                }
            }
        }
        found
    }
}

#[cfg(unix)]
fn has_other_names(path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.nlink() > 1)
}

#[cfg(not(unix))]
fn has_other_names(_path: &Path) -> bool {
    false
}

/// Whether the platform lets the harness create symbolic links. Where it does not, no fixture
/// has one and the mutation that replaces an entry with one changes nothing.
const SYMLINKS: bool = cfg!(unix);

#[cfg(unix)]
fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn symlink(_target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::other("symbolic links are not created here"))
}

/// Whether `first` and `second` are one entry right now: the same path, or two names of one file.
/// Both identities are read at the same moment, so a file system that reuses identities cannot
/// make two different entries look like one.
fn same_entry(first: &Path, second: &Path) -> bool {
    if first == second {
        return true;
    }
    let (Ok(first_metadata), Ok(second_metadata)) =
        (fs::symlink_metadata(first), fs::symlink_metadata(second))
    else {
        return false;
    };
    match (
        file_id_of(first, &first_metadata),
        file_id_of(second, &second_metadata),
    ) {
        (Ok(first_id), Ok(second_id)) => first_id == second_id,
        _ => false,
    }
}

/// What a mutation did.
pub struct Applied {
    /// What happened, for the trace.
    pub effect: String,
    /// The names that now hold an entry that is not the one they held, though a snapshot may see
    /// an entry there before and after: the harness replaced the entry, or moved another into the
    /// name. A name is listed only once the change has happened and left another entry in it: an
    /// operation that failed, or that changed nothing (a rename onto itself, or onto another name
    /// of the same file), lists none, and neither does one that unlinked a name and linked the
    /// same file under it again.
    pub touched: Vec<PathBuf>,
}

/// Applies `op` to the live tree and says what it did. Which entries it acts on follows from the
/// tree as `snapshot` shows it. It touches nothing outside the scan root, so a sentinel is never
/// the subject of a mutation, though a mutation may link to one. An operation the tree does not
/// allow, or the platform does not support, does nothing; the model learns what changed from the
/// tree, not from this text.
pub fn apply(world: &World, snapshot: &Snapshot, op: Op) -> Applied {
    let candidates = Candidates::of(world, snapshot);
    let mut touched = Vec::new();
    let result = apply_to(world, &candidates, op, &mut touched);
    Applied {
        effect: result.unwrap_or_else(|error| format!("{op}: refused ({error})")),
        touched,
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one flat match over the kinds of mutation"
)]
fn apply_to(
    world: &World,
    candidates: &Candidates,
    op: Op,
    touched: &mut Vec<PathBuf>,
) -> io::Result<String> {
    let nothing = || Ok(format!("{op}: nothing to act on"));
    match op.kind {
        MutationKind::CreateFile | MutationKind::CreateFolder => {
            let Some(parent) = pick(&candidates.folders, op.a) else {
                return nothing();
            };
            let path = fresh(parent, op.c);
            if op.kind == MutationKind::CreateFolder {
                fs::create_dir(&path)?;
                return Ok(format!("created folder {}", path.display()));
            }
            let length = LENGTHS[select(op.b, LENGTHS.len())];
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            file.write_all(&vec![b'n'; length])?;
            Ok(format!("created file {} ({length} bytes)", path.display()))
        }
        MutationKind::Remove => {
            let Some((path, kind)) = pick(&candidates.entries, op.a) else {
                return nothing();
            };
            match (kind, op.b & 1) {
                (Kind::Folder, 0) => fs::remove_dir(path)?,
                (Kind::Folder, _) => fs::remove_dir_all(path)?,
                _ => fs::remove_file(path)?,
            }
            Ok(format!("removed {}", path.display()))
        }
        MutationKind::Rename => {
            let (Some((source, _)), Some(folder)) = (
                pick(&candidates.entries, op.a),
                pick(&candidates.folders, op.b),
            ) else {
                return nothing();
            };
            let destination = folder.join(FRESH[select(op.c, FRESH.len())]);
            if same_entry(source, &destination) {
                return Ok(format!(
                    "{op}: nothing to act on ({} is already {})",
                    source.display(),
                    destination.display()
                ));
            }
            fs::rename(source, &destination)?;
            touched.push(destination.clone());
            Ok(format!(
                "renamed {} to {}",
                source.display(),
                destination.display()
            ))
        }
        MutationKind::FileToFolder => {
            let Some(path) = pick(&candidates.non_folders, op.a) else {
                return nothing();
            };
            fs::remove_file(path)?;
            fs::create_dir(path)?;
            touched.push(path.clone());
            if op.b & 1 != 0 {
                write_file(&path.join("inner"), 100)?;
            }
            Ok(format!("replaced {} with a folder", path.display()))
        }
        MutationKind::FileToSymlink => {
            if !SYMLINKS {
                return Ok(format!(
                    "{op}: nothing to act on (symbolic links are not created on this platform)"
                ));
            }
            let Some(path) = pick(&candidates.non_folders, op.a) else {
                return nothing();
            };
            let targets = [
                world.outside.join("sentinel-dir"),
                world.outside.join("sentinel-file"),
                PathBuf::from("nowhere-at-all"),
                world.root.clone(),
            ];
            let target = &targets[select(op.b, targets.len())];
            fs::remove_file(path)?;
            symlink(target, path)?;
            touched.push(path.clone());
            Ok(format!(
                "replaced {} with a link to {}",
                path.display(),
                target.display()
            ))
        }
        MutationKind::HardLinkAdd => {
            let (Some(source), Some(folder)) = (
                pick(&candidates.files, op.a),
                pick(&candidates.folders, op.b),
            ) else {
                return nothing();
            };
            let destination = fresh(folder, op.c);
            fs::hard_link(source, &destination)?;
            Ok(format!(
                "linked {} as {}",
                source.display(),
                destination.display()
            ))
        }
        MutationKind::HardLinkRemove => {
            let Some(path) = pick(&candidates.linked, op.a) else {
                return nothing();
            };
            fs::remove_file(path)?;
            Ok(format!("removed the hard link {}", path.display()))
        }
        MutationKind::Grow => {
            let Some(path) = pick(&candidates.files, op.a) else {
                return nothing();
            };
            let length = GROWTH[select(op.c, GROWTH.len())];
            let mut file = fs::OpenOptions::new().append(true).open(path)?;
            file.write_all(&vec![b'g'; length])?;
            Ok(format!("grew {} by {length} bytes", path.display()))
        }
        MutationKind::FileToHardLink => {
            let (Some(path), Some(other)) =
                (pick(&candidates.files, op.a), pick(&candidates.files, op.b))
            else {
                return nothing();
            };
            if path == other {
                return nothing();
            }
            // Two names of one file: the name is unlinked and linked again, and holds the same
            // entry afterwards.
            let same_file = same_entry(path, other);
            fs::remove_file(path)?;
            fs::hard_link(other, path)?;
            if !same_file {
                touched.push(path.clone());
            }
            Ok(format!(
                "replaced {} with a hard link to {}",
                path.display(),
                other.display()
            ))
        }
    }
}
