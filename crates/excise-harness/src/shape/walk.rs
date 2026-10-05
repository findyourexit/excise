//! The walk: a read-only pass over a tree that keeps its aggregates and nothing else.
//!
//! [`profile`] opens the root, lists each folder, inspects every entry with `lstat`, and adds what
//! it learns to counters, then forgets the entry. The walk writes nothing, opens no file, and
//! reads no content of any file. (`excise-shape profile --output` makes its one new file only
//! after the walk has ended; see [`super::cli`].)
//!
//! # Memory
//!
//! The walk holds exactly what follows, and all of it is in memory: nothing is spilled to disk,
//! so a tree for which any of the first three does not fit in memory cannot be profiled. Those
//! three grow with the tree, each with a different part of it, and the walk's memory grows with
//! them and with nothing else of the tree: the rest is bounded whatever the tree is.
//!
//! * **The names of the folder being listed**, all of them at once, whatever they name: a folder
//!   is read to its end before its first entry is counted. Each name is measured and dropped as
//!   its entry is counted, so the name of a file, a link, or anything that is not a folder does
//!   not outlive the listing. This grows with the widest folder, which is what costs: one of a
//!   million files with names of 24 bytes took 73 MB at its peak on macOS, about 66 bytes an
//!   entry, and a million entries in folders of a thousand cost a thousand names at a time.
//! * **The names of the subfolders still to be visited, in each folder on the current path**,
//!   from the root to the folder being walked. A folder that has been listed keeps those names
//!   and no others until each subfolder has been entered. About 100 bytes a name, from the sizes
//!   of what holds it (a record of 48 bytes, and the name's own allocation: an estimate, not a
//!   measurement). This grows with the subfolders that wait along one path, down to
//!   [`MAX_PROFILE_DEPTH`] levels, and not only with those of the widest folder: in a comb, a
//!   chain of folders that each hold many subfolders besides the next one of the chain, the
//!   folders waiting along the chain can approach the number of folders in the tree.
//! * **The identity of every file that has more than one name**, from the first of its names the
//!   walk meets until the walk ends: its device and inode number, how many names the file system
//!   says it has, how many the walk met, and the class of its size, 40 bytes, and about 60 with
//!   the slack of the tree they are kept in (an estimate from the sizes of the structures, not a
//!   measurement). A name is never kept: the walk keeps the file's identity and counts its names.
//!   This grows with the number of such files, up to every file of the tree.
//! * **The name of each folder on the current path**, so that the walk can open a folder it gave
//!   up the handle of again by name, and **a fixed set of counters and histograms**, of a few
//!   hundred buckets at most, whatever the tree. These are bounded: the names on the path are one
//!   for each of at most [`MAX_PROFILE_DEPTH`] levels.
//!
//! So the walk costs the widest folder while it is listed, plus the subfolders still to be
//! visited in each folder on the current path, plus the identities of files with more than one
//! name, and the handles of [`HANDLE_BUDGET`] folders at most (see below).
//!
//! # What the walk will not do
//!
//! * It never follows a link below the root. A symbolic link is an entry of its own: the walk
//!   neither enters it, nor reads where it points, nor asks whether what it points at exists,
//!   which would take it out of the tree (a call that follows a link can start an automount, or
//!   wait on a mount that does not answer). The only question it puts about an entry is `lstat`
//!   of the entry itself, relative to the folder that holds it, and about a folder it has opened,
//!   `fstat` of its handle.
//! * It stays on the file system of the root unless asked to cross. A folder on another file
//!   system is counted, as a folder, and not entered; the profile counts how many it skipped.
//!   Where the platform reports no device (Windows), the walk cannot tell, and a mount point or
//!   a junction, which is a reparse point that names another place, is a link to it and not a
//!   folder.
//! * It never stops at an entry it cannot read. A folder it cannot open or list, and an entry
//!   that disappears or fails to be inspected, are counted and the walk goes on. Only the root
//!   must be readable, because without it there is nothing to describe.
//! * It never keeps a name beyond the listing or the visit it is for. A name is measured (its
//!   length in bytes, as WTF-8 on Windows, where a name is UTF-16 that may hold a surrogate that
//!   is half of no pair: 3 bytes for each such surrogate, and for a valid name the length of its
//!   UTF-8) and dropped, a hard-linked file is remembered by its device and inode number and
//!   never by a name, and a link target is never read.
//!
//! # The root
//!
//! The path the walk is given is resolved like any path a person types: a link among its
//! components is followed, as every program follows it. What is checked is the last component,
//! which must be a folder itself and not a link to one, on Unix and on Windows: on Unix it is
//! opened without following a link, and on Windows it is opened as a reparse point and what was
//! opened is looked at. Nothing below the root is ever followed.
//!
//! A separator after the last component, and a `.` after it, make no difference: `link/`,
//! `link//`, and `link/.` are `link`, and the entry that is checked is the one the walk opens
//! without any of them. A system resolves a link that a path names with a trailing separator
//! whatever the flags of the open say, because it must resolve it to know whether the name is of
//! a folder, so the walk takes the separators off before it opens the root. On Windows that holds
//! for a path in the verbatim form too, `\\?\C:\...\link\.` and `\\?\C:\...\link\.\`: std keeps
//! each `.` of such a path as a component of its own, one that ends it as well, instead of
//! dropping it, and the walk takes those components off, as many as there are, so that the name
//! before them is the one it opens. A path that has no last name to take them off (`/`, a drive,
//! a share, `.`, `..`) is opened as it is.
//!
//! # Folders that change under the walk
//!
//! A folder is inspected when its parent is listed and opened later, when the walk gets to it,
//! and in between it can be replaced. The walk takes what it decides from the folder it has
//! opened, and not from what it inspected: it opens the folder without following a link, and
//! takes what it can tell about it from the handle itself.
//!
//! * On Unix that is the device and inode of what it opened: a folder that is not the one it
//!   inspected is counted as an entry that changed during the walk, in `unreadable.errors` with
//!   the entries that vanished, and is not listed.
//! * On Windows it opens every folder it walks as a reparse point and refuses a link or a
//!   junction in the place of a folder, and that is the whole of the check: stable Rust has no
//!   file ID to compare (`MetadataExt::file_index` is unstable) and this crate has no `unsafe`
//!   code, so a folder replaced by another ordinary folder in that window is walked, and what it
//!   holds is counted. That is the tree changing under the walk, which no walk of a live tree
//!   prevents, and nothing outside the tree is reached through a link. The walk does hold every
//!   folder it is inside open, with the right to list it and every sharing mode but delete, for
//!   as long as it is inside it: none of those can be renamed, deleted, or replaced by a
//!   junction while the walk runs. A folder it has not opened yet is not held.
//!
//! # Handles
//!
//! Every folder is reached by name from the folder that holds it, so how deep a folder is does
//! not limit the walk, down to [`MAX_PROFILE_DEPTH`] levels. On Unix the walk keeps at most
//! [`HANDLE_BUDGET`] folder handles open at once however deep the tree is, and a listing opens
//! one more for as long as it reads. It keeps the handle of the root, which it opens other
//! folders again from, and of the folders it has been in most recently; it gives up the others,
//! and a folder that has no subfolder left to enter gives up its own at once. When it comes back
//! to a folder whose handle it gave up, it opens it again by name, without following a link, from
//! the nearest folder above it that it still holds, and checks that every folder it opened on the
//! way is the one it recorded (the same device and inode): one that is not is counted, like a
//! folder that changed, and the folder that was to be entered from it is not walked. The
//! budget is well below the 256 descriptors that are a process's soft limit in a macOS shell, so
//! that the standard streams, the output file, and a listing's own descriptor leave it room. On
//! Windows the limit is not a few hundred, and the walk holds every folder it is inside, so it
//! has no budget.

use std::{
    cell::Cell,
    collections::{BTreeMap, VecDeque},
    io,
    ops::Deref,
    path::Path,
    rc::Rc,
};

use thiserror::Error;

use super::dir::{Name, WalkDir, name_len};
use crate::{
    fixture::{NodeKind, sys::Stat},
    histogram::{Buckets, MAX_NAME_LENGTH, class_floor},
    report::{
        HarnessShapeProfile, MAX_PROFILE_DEPTH, SchemaVersion, ShapeDepth, ShapeEntries,
        ShapeHardLinks, ShapeHistogram, ShapeNameLengths, ShapePlatform, ShapeProfileKind,
        ShapeSymbolicLinks, ShapeUnreadable, ShapeWalk,
    },
};

/// The most folder handles the walk keeps open at once on Unix, whatever the depth of the tree:
/// the root's, the folder being opened, and the ones it keeps for the folders it was in last. A
/// listing opens one more while it reads. The soft limit on descriptors that a shell gives a
/// process is 256 on macOS, so this leaves most of it to everything else.
pub const HANDLE_BUDGET: usize = 32;

/// How many folders other than the root the walk keeps a handle for: the budget less the root's
/// handle and the two that opening a folder again can need besides (the one it opens from and
/// the one it opens), and at least one.
const fn window_for(budget: usize) -> usize {
    let window = budget.saturating_sub(3);
    if window == 0 { 1 } else { window }
}

/// The window of a walk that has no budget: every folder it is inside stays held, and none is
/// given up early. That is every walk on Windows, where the walk holds the folders it is inside
/// so that none can be renamed, deleted, or replaced.
const NO_BUDGET: usize = usize::MAX;

/// How to walk.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalkOptions {
    /// Whether to enter folders on other file systems. By default the walk stays on the file
    /// system of the root.
    pub cross_filesystems: bool,
}

/// Why a tree could not be profiled. No message of this type names a path: the root is what the
/// person who ran the command typed, and nothing below it is told to anyone.
#[derive(Debug, Error)]
pub enum WalkError {
    /// The root cannot be opened as a folder.
    #[error("cannot open the root: {source} (it must be a folder, not a link to one)")]
    Root {
        /// The underlying error.
        source: io::Error,
    },
    /// The root cannot be listed.
    #[error("cannot list the root: {source}")]
    List {
        /// The underlying error.
        source: io::Error,
    },
}

/// Walks the tree at `root` and returns its shape.
///
/// # Errors
///
/// Returns [`WalkError`] when the root cannot be opened or listed. Nothing below the root can fail
/// the walk: what it cannot read it counts.
pub fn profile(root: &Path, options: WalkOptions) -> Result<HarnessShapeProfile, WalkError> {
    run_walk(root, options, |_| {}).map(|(shape, _, _)| shape)
}

/// Walks the tree, after `set_up` has had a look at the walker, and returns its shape, the most
/// folder handles it had open at once, and how many it opened in all.
fn run_walk(
    root: &Path,
    options: WalkOptions,
    set_up: impl FnOnce(&mut Walker),
) -> Result<(HarnessShapeProfile, usize, usize), WalkError> {
    let directory = WalkDir::open_root(root).map_err(|source| WalkError::Root { source })?;
    let own = directory
        .stat_self()
        .map_err(|source| WalkError::Root { source })?;
    let mut walker = Walker::new(options, own.dev);
    set_up(&mut walker);
    walker
        .run(directory, &own)
        .map_err(|source| WalkError::List { source })?;
    let (peak, opened) = (walker.handles.peak.get(), walker.handles.opened.get());
    Ok((walker.finish(options, own.ino.is_some()), peak, opened))
}

/// What a test does just before the walk opens a folder, given its depth and its name: it changes
/// the tree there.
#[cfg(test)]
type BeforeOpen = Box<dyn FnMut(u32, &Name)>;

/// What a test changes in a walk: the handle budget, and what happens to the tree between the
/// inspection of a folder and the opening of it.
#[cfg(test)]
pub(super) struct Seam {
    pub(super) budget: usize,
    pub(super) before_open: Option<BeforeOpen>,
}

/// [`profile`] under a [`Seam`], with the most folder handles the walk had open at once and the
/// number of handles it opened in all (more than the folders it walked when it opened some
/// again).
///
/// # Errors
///
/// As [`profile`].
#[cfg(test)]
pub(super) fn profile_under(
    root: &Path,
    options: WalkOptions,
    seam: Seam,
) -> Result<(HarnessShapeProfile, usize, usize), WalkError> {
    run_walk(root, options, |walker| {
        walker.window = window_for(seam.budget);
        walker.before_open = seam.before_open;
    })
}

/// A histogram being filled.
#[derive(Debug, Default)]
struct Tally {
    count: u64,
    total: u64,
    max: u64,
    buckets: Buckets,
}

impl Tally {
    /// Counts `value` in its class of powers of two.
    fn class(&mut self, value: u64) {
        self.record(value, class_floor(value));
    }

    /// Counts a name of `length` bytes in the bucket of its length.
    fn length(&mut self, length: usize) {
        let length = u64::try_from(length)
            .unwrap_or(MAX_NAME_LENGTH)
            .clamp(1, MAX_NAME_LENGTH);
        self.record(length, length);
    }

    fn record(&mut self, value: u64, key: u64) {
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(value);
        self.max = self.max.max(value);
        self.buckets.add(key, 1);
    }

    fn into_histogram(self) -> ShapeHistogram {
        ShapeHistogram {
            count: self.count,
            total: self.total,
            max: self.max,
            buckets: self.buckets,
        }
    }
}

/// What the walk remembers about a file with more than one name: how many names the file system
/// says it has, how many the walk met, and the class of the size the first of them had. Never a
/// name. The walk keeps one of these, under the device and inode number of the file (16 bytes), for
/// every such file until it ends: 40 bytes, and about 60 with the slack of the tree that holds
/// them (an estimate from the sizes of the structures, not a measurement).
///
/// The names of a file have one size, so they lie in one class of `file_sizes`. A name met with
/// another size, because the file changed while the walk ran, is a file of another class and is
/// not counted as a name of the group: the names of a group are always among the files of its
/// class, which `file_sizes` counts name by name.
#[derive(Debug, Clone, Copy)]
struct Identity {
    links: u64,
    seen: u64,
    class: u64,
}

/// What one folder holds, counted as its entries go by.
#[derive(Debug, Default, Clone, Copy)]
struct Held {
    children: u64,
    directories: u64,
    files: u64,
}

/// How many folder handles the walk has open, the most it has had open at once, and how many it
/// has opened.
#[derive(Debug, Default)]
struct Handles {
    open: Cell<usize>,
    peak: Cell<usize>,
    opened: Cell<usize>,
}

/// An open folder, counted for as long as it is open.
struct Handle {
    dir: WalkDir,
    handles: Rc<Handles>,
}

impl Handle {
    fn new(dir: WalkDir, handles: &Rc<Handles>) -> Self {
        let open = handles.open.get() + 1;
        handles.open.set(open);
        handles.peak.set(handles.peak.get().max(open));
        handles.opened.set(handles.opened.get() + 1);
        Self {
            dir,
            handles: Rc::clone(handles),
        }
    }
}

impl Deref for Handle {
    type Target = WalkDir;

    fn deref(&self) -> &WalkDir {
        &self.dir
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.handles
            .open
            .set(self.handles.open.get().saturating_sub(1));
    }
}

/// A subfolder that is still to be entered: its name, and the device and inode it had when it
/// was inspected, when the platform says.
struct Pending {
    name: Name,
    identity: Option<(u64, u64)>,
}

/// A folder on the path from the root to the one being walked, with what is left to do in it.
struct Frame {
    /// How the folder is named in the one above it; nothing for the root.
    name: Name,
    /// The device and inode of the folder when the walk first opened it, when the platform
    /// says: it is on the walker's path until the frame is done, and it is what the folder must
    /// still be when the walk opens it again.
    identity: Option<(u64, u64)>,
    /// The folder's handle, while the walk holds one.
    handle: Option<Handle>,
    /// The subfolders not yet entered. They are dropped as they are used.
    pending: Vec<Pending>,
    depth: u32,
}

/// What the walk decides about a folder it is about to enter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Enter,
    /// A folder on another file system, not asked for.
    MountPoint,
    /// A folder that is its own ancestor, or deeper than the walk goes.
    Unwalkable,
}

/// The device and inode of an entry, when the platform says both.
fn identity_of(stat: &Stat) -> Option<(u64, u64)> {
    Some((stat.dev?, stat.ino?))
}

/// What a folder that is not the one the walk inspected, or opened before, is counted as.
fn changed() -> io::Error {
    io::Error::other("the folder changed while the walk was in it")
}

struct Walker {
    /// The identities of the folders from the root down to the one being listed.
    path: Vec<(u64, u64)>,
    cross_filesystems: bool,
    root_device: Option<u64>,
    /// How many folders other than the root the walk keeps a handle for at once.
    window: usize,
    handles: Rc<Handles>,
    levels: Vec<ShapeDepth>,
    children: Tally,
    subdirectories: Tally,
    files: Tally,
    sizes: Tally,
    directory_names: Tally,
    file_names: Tally,
    symlink_names: Tally,
    identities: BTreeMap<(u64, u64), Identity>,
    mount_points_skipped: u64,
    unreadable_directories: u64,
    errors: u64,
    /// Called with the depth and the name of each folder just before the walk opens it: a test
    /// changes the tree there.
    #[cfg(test)]
    before_open: Option<BeforeOpen>,
}

impl Walker {
    fn new(options: WalkOptions, root_device: Option<u64>) -> Self {
        Self {
            path: Vec::new(),
            cross_filesystems: options.cross_filesystems,
            root_device,
            window: if cfg!(windows) {
                NO_BUDGET
            } else {
                window_for(HANDLE_BUDGET)
            },
            handles: Rc::new(Handles::default()),
            levels: Vec::new(),
            children: Tally::default(),
            subdirectories: Tally::default(),
            files: Tally::default(),
            sizes: Tally::default(),
            directory_names: Tally::default(),
            file_names: Tally::default(),
            symlink_names: Tally::default(),
            identities: BTreeMap::new(),
            mount_points_skipped: 0,
            unreadable_directories: 0,
            errors: 0,
            #[cfg(test)]
            before_open: None,
        }
    }

    /// Visits the root, then every folder below it, depth first.
    fn run(&mut self, root: WalkDir, own: &Stat) -> io::Result<()> {
        let root = Handle::new(root, &self.handles);
        let mut stack: Vec<Frame> = Vec::new();
        // The frames above the root that hold a handle, shallowest first.
        let mut kept: VecDeque<usize> = VecDeque::new();
        stack.extend(self.list(root, 0, Name::default(), identity_of(own))?);
        while let Some(top) = stack.len().checked_sub(1) {
            let Some(next) = stack[top].pending.pop() else {
                if let Some(frame) = stack.pop()
                    && frame.identity.is_some()
                {
                    self.path.pop();
                }
                if kept.back() == Some(&top) {
                    kept.pop_back();
                }
                continue;
            };
            match self.enter(&mut stack, &mut kept, top, next) {
                Ok(Some(frame)) => {
                    kept.push_back(stack.len());
                    stack.push(frame);
                    while kept.len() > self.window {
                        if let Some(old) = kept.pop_front() {
                            stack[old].handle = None;
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => self.note_unreadable(&error),
            }
            // Once its last subfolder is open or given up on, nothing in this folder needs its
            // handle, unless the walk has no budget and holds every folder it is inside. The root
            // keeps its own: it is what the others are opened again from.
            if self.window != NO_BUDGET
                && top > 0
                && stack[top].pending.is_empty()
                && stack[top].handle.take().is_some()
            {
                kept.retain(|index| *index != top);
            }
        }
        Ok(())
    }

    /// A folder, or an entry in it, that could not be read.
    fn note_unreadable(&mut self, error: &io::Error) {
        if error.kind() == io::ErrorKind::PermissionDenied {
            self.unreadable_directories += 1;
        } else {
            self.errors += 1;
        }
    }

    /// Opens the subfolder `next` of the folder `top` of the stack, and lists it. The folder `top`
    /// is opened again first when the walk gave up its handle.
    fn enter(
        &mut self,
        stack: &mut [Frame],
        kept: &mut VecDeque<usize>,
        top: usize,
        next: Pending,
    ) -> io::Result<Option<Frame>> {
        if stack[top].handle.is_none()
            && let Err(error) = self.reopen(stack, kept, top)
        {
            // A folder that cannot be entered again cannot be entered from either.
            stack[top].pending.clear();
            return Err(error);
        }
        let depth = stack[top].depth + 1;
        #[cfg(test)]
        {
            if let Some(hook) = self.before_open.as_mut() {
                hook(depth, &next.name);
            }
        }
        let Some(parent) = stack[top].handle.as_ref() else {
            return Err(changed());
        };
        let child = Handle::new(parent.open_child(&next.name)?, &self.handles);
        // What was opened is judged by its own handle, not by what was inspected before. Where the
        // platform gives no device and inode (Windows), both sides are `None` and this passes for
        // any folder: what `open_child` checked, that it is a folder and not a link or a junction,
        // is all there is, and another ordinary folder put in the place of the one inspected is
        // walked.
        let opened = child.stat_self()?;
        let found = identity_of(&opened);
        if found != next.identity {
            return Err(changed());
        }
        match self.verdict(opened.dev, opened.ino, depth) {
            Verdict::Enter => {}
            Verdict::MountPoint => {
                self.mount_points_skipped += 1;
                return Ok(None);
            }
            Verdict::Unwalkable => {
                self.errors += 1;
                return Ok(None);
            }
        }
        self.list(child, depth, next.name, found)
    }

    /// Opens the folder `target` of the stack again, by name and without following a link, from
    /// the nearest folder above it that still has a handle, checking that every folder on the
    /// way is the one that was recorded. It keeps the handles of the last folders on the way,
    /// as many as the window holds, so that the folders it returns to next need no reopening.
    fn reopen(
        &mut self,
        stack: &mut [Frame],
        kept: &mut VecDeque<usize>,
        target: usize,
    ) -> io::Result<()> {
        let anchor = (0..target)
            .rev()
            .find(|index| stack[*index].handle.is_some())
            .unwrap_or(0);
        let first_kept = target
            .saturating_sub(self.window.saturating_sub(1))
            .max(anchor + 1);
        // A handle on the way that is not kept, and is only passed through.
        let mut passing: Option<Handle> = None;
        for level in anchor + 1..=target {
            let parent = match &passing {
                Some(handle) => handle,
                None => stack[level - 1].handle.as_ref().ok_or_else(changed)?,
            };
            let child = Handle::new(parent.open_child(&stack[level].name)?, &self.handles);
            if identity_of(&child.stat_self()?) != stack[level].identity {
                return Err(changed());
            }
            if level >= first_kept && (level == target || !stack[level].pending.is_empty()) {
                stack[level].handle = Some(child);
                kept.push_back(level);
                passing = None;
            } else {
                passing = Some(child);
            }
        }
        while kept.len() > self.window {
            if let Some(old) = kept.pop_front() {
                stack[old].handle = None;
            }
        }
        Ok(())
    }

    /// Whether a folder with this device and inode, at `depth`, is to be entered: from what was
    /// inspected when it was listed, and again from the handle it was opened by.
    fn verdict(&self, dev: Option<u64>, ino: Option<u64>, depth: u32) -> Verdict {
        let elsewhere = !self.cross_filesystems
            && matches!(
                (dev, self.root_device),
                (Some(device), Some(root)) if device != root
            );
        // A folder that is one of its own ancestors, which a bind mount can make, would be
        // walked for ever.
        let looped = matches!(
            (dev, ino),
            (Some(device), Some(inode)) if self.path.contains(&(device, inode))
        );
        if elsewhere {
            Verdict::MountPoint
        } else if looped || depth >= MAX_PROFILE_DEPTH {
            Verdict::Unwalkable
        } else {
            Verdict::Enter
        }
    }

    /// Lists `directory`, which is at `depth`, and counts everything in it as it goes, so that the
    /// name of a file, a link, or anything that is not a folder is dropped as soon as its entry is
    /// counted. It returns the folder again with the names of its subfolders still to be entered,
    /// and nothing else of what it held, unless it has none.
    fn list(
        &mut self,
        directory: Handle,
        depth: u32,
        name: Name,
        identity: Option<(u64, u64)>,
    ) -> io::Result<Option<Frame>> {
        let children = directory.list()?;
        self.path.extend(identity);
        let child_depth = depth + 1;
        let mut held = Held::default();
        let mut pending = Vec::new();
        for child in children {
            let Ok(stat) = directory.stat(&child) else {
                self.errors += 1;
                continue;
            };
            held.children += 1;
            self.entry(child, &stat, child_depth, &mut held, &mut pending);
        }
        self.children.class(held.children);
        self.subdirectories.class(held.directories);
        self.files.class(held.files);
        if pending.is_empty() {
            if identity.is_some() {
                self.path.pop();
            }
            return Ok(None);
        }
        Ok(Some(Frame {
            name,
            identity,
            handle: Some(directory),
            pending,
            depth,
        }))
    }

    /// The level of `depth`, created, with every level above it, when it is the deepest so far.
    fn level(&mut self, depth: u32) -> &mut ShapeDepth {
        let wanted = usize::try_from(depth).unwrap_or(usize::MAX);
        while self.levels.len() < wanted {
            let next = u32::try_from(self.levels.len() + 1).unwrap_or(u32::MAX);
            self.levels.push(ShapeDepth {
                depth: next,
                directories: 0,
                files: 0,
                symlinks: 0,
                others: 0,
                bytes: 0,
            });
        }
        &mut self.levels[wanted - 1]
    }

    /// Counts one entry of the folder being listed: `name` is only measured, and a subfolder to
    /// enter is handed on to `pending`, which takes the name. The name of anything else is dropped
    /// here.
    fn entry(
        &mut self,
        name: Name,
        stat: &Stat,
        depth: u32,
        held: &mut Held,
        pending: &mut Vec<Pending>,
    ) {
        match stat.kind {
            NodeKind::Directory => {
                held.directories += 1;
                self.level(depth).directories += 1;
                self.directory_names.length(name_len(&name));
                match self.verdict(stat.dev, stat.ino, depth) {
                    Verdict::Enter => pending.push(Pending {
                        name,
                        identity: identity_of(stat),
                    }),
                    Verdict::MountPoint => self.mount_points_skipped += 1,
                    Verdict::Unwalkable => self.errors += 1,
                }
            }
            NodeKind::File => {
                held.files += 1;
                let level = self.level(depth);
                level.files += 1;
                level.bytes = level.bytes.saturating_add(stat.size);
                self.sizes.class(stat.size);
                self.file_names.length(name_len(&name));
                if let (Some(device), Some(inode), Some(links)) = (stat.dev, stat.ino, stat.nlink)
                    && links > 1
                {
                    let class = class_floor(stat.size);
                    let identity = self.identities.entry((device, inode)).or_insert(Identity {
                        links,
                        seen: 0,
                        class,
                    });
                    // A name of another size than the first met is not a name of the group.
                    if identity.class == class {
                        identity.seen = identity.seen.saturating_add(1);
                    }
                }
            }
            NodeKind::Symlink => {
                self.level(depth).symlinks += 1;
                self.symlink_names.length(name_len(&name));
            }
            NodeKind::Other => self.level(depth).others += 1,
        }
    }

    fn finish(self, options: WalkOptions, identity: bool) -> HarnessShapeProfile {
        let mut entries = ShapeEntries {
            total: 0,
            directories: 0,
            files: 0,
            symlinks: 0,
            others: 0,
        };
        for level in &self.levels {
            entries.directories += level.directories;
            entries.files += level.files;
            entries.symlinks += level.symlinks;
            entries.others += level.others;
        }
        entries.total = entries.directories + entries.files + entries.symlinks + entries.others;

        let mut group_sizes = Tally::default();
        let mut by_file_size: BTreeMap<u64, Tally> = BTreeMap::new();
        let mut names = 0_u64;
        let mut incomplete_groups = 0_u64;
        let groups = self.identities.len() as u64;
        for linked in self.identities.values() {
            names = names.saturating_add(linked.seen);
            incomplete_groups += u64::from(linked.seen < linked.links);
            group_sizes.class(linked.seen);
            by_file_size
                .entry(linked.class)
                .or_default()
                .class(linked.seen);
        }

        HarnessShapeProfile {
            document_kind: ShapeProfileKind::HarnessShapeProfile,
            schema_version: SchemaVersion,
            platform: ShapePlatform {
                os: std::env::consts::OS.to_owned(),
                identity,
            },
            walk: ShapeWalk {
                cross_filesystems: options.cross_filesystems,
                mount_points_skipped: self.mount_points_skipped,
            },
            entries,
            max_depth: u32::try_from(self.levels.len()).unwrap_or(u32::MAX),
            depths: self.levels,
            children_per_directory: self.children.into_histogram(),
            subdirectories_per_directory: self.subdirectories.into_histogram(),
            files_per_directory: self.files.into_histogram(),
            file_sizes: self.sizes.into_histogram(),
            name_lengths: ShapeNameLengths {
                directories: self.directory_names.into_histogram(),
                files: self.file_names.into_histogram(),
                symlinks: self.symlink_names.into_histogram(),
            },
            hard_links: ShapeHardLinks {
                groups,
                names,
                incomplete_groups,
                group_sizes: group_sizes.into_histogram(),
                group_sizes_by_file_size: by_file_size
                    .into_iter()
                    .map(|(class, tally)| (class, tally.into_histogram()))
                    .collect(),
            },
            symbolic_links: ShapeSymbolicLinks {
                count: entries.symlinks,
            },
            unreadable: ShapeUnreadable {
                directories: self.unreadable_directories,
                errors: self.errors,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    /// A walker told that the root is on a device that none of the folders below it is on, which
    /// is what every mount point looks like, counts those folders and does not enter them unless
    /// it is asked to cross. This is the decision the privileged volume test
    /// (`EXCISE_HARNESS_PRIVILEGED=1`) makes with a real mount point, made without one.
    #[test]
    fn a_folder_on_another_device_is_counted_and_entered_only_when_asked() {
        let scratch = tempfile::tempdir().expect("a directory");
        fs::create_dir_all(scratch.path().join("a/inner")).expect("folders");
        fs::create_dir(scratch.path().join("b")).expect("a folder");
        fs::write(scratch.path().join("a/inner/file"), "x").expect("a file");
        fs::write(scratch.path().join("top"), "x").expect("a file");

        let walk = |cross_filesystems: bool| {
            let options = WalkOptions { cross_filesystems };
            let root = WalkDir::open_root(scratch.path()).expect("the root opens");
            let own = root.stat_self().expect("the root has a status");
            // The walker believes the root is on another device than the folders really are.
            let device = own.dev?;
            let mut walker = Walker::new(options, Some(device ^ 1));
            walker.run(root, &own).expect("the walk");
            Some(walker.finish(options, own.ino.is_some()))
        };
        // A platform that reports no device (Windows) can find no mount point.
        let (Some(staying), Some(crossing)) = (walk(false), walk(true)) else {
            return;
        };

        // The two folders at the top are counted as folders, and nothing below them is.
        assert_eq!(staying.walk.mount_points_skipped, 2);
        assert_eq!(staying.entries.directories, 2);
        assert_eq!(staying.entries.files, 1);
        assert_eq!(staying.max_depth, 1);
        // Asked to cross, the walk enters every folder and skips none.
        assert_eq!(crossing.walk.mount_points_skipped, 0);
        assert_eq!(crossing.entries.directories, 3);
        assert_eq!(crossing.entries.files, 2);
        assert_eq!(crossing.max_depth, 3);
        staying.check().expect("a consistent profile");
        crossing.check().expect("a consistent profile");
    }

    #[test]
    fn a_budget_is_never_smaller_than_the_walk_needs() {
        assert_eq!(window_for(HANDLE_BUDGET), HANDLE_BUDGET - 3);
        assert_eq!(window_for(4), 1);
        assert_eq!(window_for(3), 1);
        assert_eq!(window_for(0), 1);
    }

    /// The name of an entry the test makes up.
    #[cfg(unix)]
    fn name(text: &str) -> Name {
        text.as_bytes().to_vec()
    }

    #[cfg(windows)]
    fn name(text: &str) -> Name {
        text.into()
    }

    /// The names of a hard-linked file have one size, unless the file changes while the walk runs.
    /// A name met with another size than the first is a file of another class, and `file_sizes`
    /// counts it there; so it is not a name of the group, which would otherwise have more names in
    /// its class than the class has files.
    #[test]
    fn a_name_met_with_another_size_than_the_first_is_not_a_name_of_its_group() {
        let scratch = tempfile::tempdir().expect("a directory");
        let root = WalkDir::open_root(scratch.path()).expect("the root opens");
        let own = root.stat_self().expect("the root has a status");
        let mut walker = Walker::new(WalkOptions::default(), own.dev);
        let file = |size: u64| Stat {
            kind: NodeKind::File,
            size,
            allocated: None,
            dev: Some(1),
            ino: Some(7),
            nlink: Some(3),
            mode: None,
            uid: None,
        };

        // Three names of one file: the first two met at 100 bytes, the third after the file had
        // grown to 5,000.
        let (mut held, mut pending) = (Held::default(), Vec::new());
        for (text, size) in [("a", 100), ("b", 100), ("c", 5_000)] {
            walker.entry(name(text), &file(size), 1, &mut held, &mut pending);
        }
        let shape = walker.finish(WalkOptions::default(), true);

        assert_eq!(shape.file_sizes.buckets.get(64), 2);
        assert_eq!(shape.file_sizes.buckets.get(4096), 1);
        let hard = &shape.hard_links;
        assert_eq!((hard.groups, hard.names, hard.incomplete_groups), (1, 2, 1));
        let in_class: Vec<_> = hard.group_sizes_by_file_size.iter().collect();
        assert_eq!(in_class.len(), 1, "{in_class:?}");
        let (class, histogram) = in_class[0];
        assert_eq!((class, histogram.count, histogram.total), (64, 1, 2));
        assert!(
            histogram.total <= shape.file_sizes.buckets.get(class),
            "the names of a class's groups are among the files of the class"
        );
    }

    /// What a folder on the path keeps while the walk is in a folder below it is the names of its
    /// subfolders that are still to be visited, and nothing of what else it held: its files and
    /// links were counted as it was listed, and their names dropped.
    #[test]
    fn a_listed_folder_keeps_the_names_of_its_subfolders_and_no_other() {
        let scratch = tempfile::tempdir().expect("a directory");
        for index in 0..40 {
            fs::write(scratch.path().join(format!("file-{index}")), "x").expect("a file");
        }
        for index in 0..3 {
            fs::create_dir(scratch.path().join(format!("folder-{index}"))).expect("a folder");
        }
        #[cfg(unix)]
        for index in 0..5 {
            std::os::unix::fs::symlink("file-0", scratch.path().join(format!("link-{index}")))
                .expect("a link");
        }
        let root = WalkDir::open_root(scratch.path()).expect("the root opens");
        let own = root.stat_self().expect("the root has a status");
        let mut walker = Walker::new(WalkOptions::default(), own.dev);
        let handle = Handle::new(root, &walker.handles);

        let frame = walker
            .list(handle, 0, Name::default(), identity_of(&own))
            .expect("the root is listed")
            .expect("it has subfolders to visit");

        assert_eq!(
            frame.pending.len(),
            3,
            "the subfolders are kept, and no file or link"
        );
        assert_eq!(walker.levels[0].files, 40);
        assert_eq!(walker.levels[0].directories, 3);
        #[cfg(unix)]
        assert_eq!(walker.levels[0].symlinks, 5);
    }
}
