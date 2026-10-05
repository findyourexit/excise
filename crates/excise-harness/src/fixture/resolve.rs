//! The path of a cache root as the system resolves it, and how long it gets on the way.
//!
//! The root of a cache need not exist yet, and a fixture in it is reached by paths that begin with
//! it: a path-based removal such as `cargo clean`, a scan, and the oracle all name it that way. So
//! [`FixtureCache::longest_entry_path_bytes`](crate::fixture::FixtureCache::longest_entry_path_bytes)
//! needs the length of the path the system works on when it reaches the fixture, and that is more
//! than the length of what was written. When the system meets a symbolic link, it puts the content
//! of the link where the name of the link was and goes on with the result: the content, and what
//! was left of the pathname after the link. macOS fails with `ENAMETOOLONG` when that result and
//! its terminating NUL do not fit in `PATH_MAX`, which is 1,024 bytes: a result of 1,023 bytes is
//! taken and one of 1,024 is refused, whatever the pathname was written as and wherever it ends
//! up. A short link to a deep directory, a link whose content goes down a long way and comes back
//! up with `..`, and a short link to a long path that ends in a link back to a short directory
//! all make it form a pathname longer than the one that was written and longer than the one it
//! resolves to.
//!
//! [`resolve`] therefore takes a path one name at a time, as the system does, and keeps the length
//! of every pathname that taking it forms, starting with the path as it is spelled: the system
//! counts the text it is handed, repeated separators and `.` names included. One thing that would
//! stop the system is no error here: a name that does not exist yet, as long as the path was
//! written with it, because the cache makes the names it needs. Every name that exists has to be
//! a folder, the last one included. A name that is not, a link that leads to a name that is not
//! there, and a link that loops mean the path cannot be resolved.

#[cfg(unix)]
use std::{
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt as _, OsStringExt as _},
};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// A path with every symbolic link in it expanded, and how long the system's pathname got while
/// it was.
#[derive(Debug)]
pub(super) struct Resolved {
    /// The path with every link expanded: the part of it that exists, as the system resolves
    /// it, and then the names that do not exist yet, as they were written. It has no `.` and no
    /// `..`.
    pub(super) path: PathBuf,
    /// The length in bytes of the longest pathname the system works on before it has the path
    /// above: the path as it is spelled, made absolute, and, each time a link is expanded, the
    /// content of the link and what was left of the pathname after it. Off Unix the links are not
    /// walked one at a time, and it is the length of the path made absolute.
    pub(super) longest: usize,
}

/// `path`, a cache root that is absolute or relative to the current directory, with every
/// symbolic link in it expanded. On Unix the root is taken as it is spelled: the system counts the
/// text it is handed, repeated separators and `.` names included, and a relative root is the
/// current directory and the text written after it.
///
/// # Errors
///
/// Returns why the path cannot be resolved: it is empty, a relative path has no current
/// directory to start from, a name on the way cannot be searched, a name that exists is not a
/// folder (the last one included), a link leads to a name that is not there, or links loop or are
/// too many.
#[cfg(unix)]
pub(super) fn resolve(path: &Path) -> io::Result<Resolved> {
    let written = spelled(path)?;
    let mut longest = written.as_os_str().len();
    let mut resolved = PathBuf::new();
    let mut pathname = Pathname::new(&written);
    let mut links = 0_u32;
    // Whether a name of the path that does not exist has been met: from there on every name is
    // taken as it was written, because nothing below a name that is not there can be a link.
    let mut absent = false;
    while let Some((step, written_with)) = pathname.next_step() {
        match step {
            Step::Root => resolved = PathBuf::from("/"),
            Step::Up if absent => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "{} is not there, so `..` cannot leave it",
                        resolved.display()
                    ),
                ));
            }
            Step::Up => {
                resolved.pop();
            }
            Step::Name(name) => {
                resolved.push(name);
                if absent {
                    continue;
                }
                match fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        links += 1;
                        if links > MAX_LINKS {
                            return Err(io::Error::other(format!(
                                "more than {MAX_LINKS} symbolic links on the way to {}",
                                written.display()
                            )));
                        }
                        let content = fs::read_link(&resolved)?.into_os_string();
                        if content.is_empty() {
                            return Err(leads_nowhere(&resolved));
                        }
                        longest = longest.max(content.len() + pathname.left());
                        // Back to the folder that holds the link, which the content is relative
                        // to unless it begins at the root.
                        resolved.pop();
                        pathname.expand(content);
                    }
                    Ok(metadata) if metadata.is_dir() => {}
                    // A name that exists has to be a folder, the last of the path included. The
                    // system refuses anything after a file, a separator, a `.`, and a `..`
                    // included, with `ENOTDIR`, and the cache has to be in a folder.
                    Ok(_) => return Err(not_a_folder(&resolved)),
                    // A name the path was written with can be made. One a link led to is a link
                    // that leads nowhere, which the cache cannot make anything of.
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        if !written_with {
                            return Err(leads_nowhere(&resolved));
                        }
                        absent = true;
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(Resolved {
        path: resolved,
        longest,
    })
}

/// `path`, a cache root that is absolute or relative to the current directory, with every
/// symbolic link in it expanded: where this is not Unix, [`std::path::absolute`] makes it
/// absolute and [`fs::canonicalize`] expands the longest ancestor of it that exists (on Windows
/// the verbatim form, `\\?\C:\...`, which is four bytes longer than the drive form), and the
/// names below it that do not exist yet follow as they were written. On Windows `absolute` is
/// `GetFullPathNameW` for a path that is not verbatim, which is the normalization (`.`, `..`,
/// repeated separators) that the standard library applies to a long path and Win32 to the rest
/// before it is used, so it gives the spelling the system sees, and a verbatim path, which
/// neither normalizes, is kept as it is.
///
/// # Errors
///
/// Returns why the path cannot be resolved: it is empty, a name on the way cannot be searched,
/// the longest ancestor that exists is not a folder, or a link leads to a name that is not there.
#[cfg(not(unix))]
pub(super) fn resolve(path: &Path) -> io::Result<Resolved> {
    let written = std::path::absolute(path)?;
    let mut existing = written.as_path();
    let mut missing = Vec::new();
    let mut resolved = loop {
        match fs::canonicalize(existing) {
            Ok(found) => break found,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // `canonicalize` says the same of a name that is not there and of a link that
                // leads nowhere, and only the first is a name the cache can make.
                match fs::symlink_metadata(existing) {
                    Ok(_) => return Err(leads_nowhere(existing)),
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                let (Some(parent), Some(name)) = (existing.parent(), existing.file_name()) else {
                    return Err(error);
                };
                missing.push(name);
                existing = parent;
            }
            Err(error) => return Err(error),
        }
    };
    // What exists has to be a folder: names written below a file cannot be made, and a root
    // that is a file cannot hold a cache.
    if !fs::metadata(&resolved)?.is_dir() {
        return Err(not_a_folder(&resolved));
    }
    resolved.extend(missing.into_iter().rev());
    Ok(Resolved {
        path: resolved,
        longest: written.as_os_str().len(),
    })
}

/// `path` made absolute without changing how it is spelled. The system counts the text it is
/// handed, and [`std::path::absolute`] would drop the `.` names and the repeated separators of
/// it, which count. A relative path is the current directory and the text after it, as
/// [`PathBuf::push`] appends it.
#[cfg(unix)]
fn spelled(path: &Path) -> io::Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot make an empty path absolute",
        ));
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let mut absolute = std::env::current_dir()?;
    absolute.push(path);
    Ok(absolute)
}

/// The error of a link that leads to `missing`, which is not there.
fn leads_nowhere(missing: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "a symbolic link leads to {}, which is not there",
            missing.display()
        ),
    )
}

/// The error of a name that exists and is not a folder.
fn not_a_folder(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::NotADirectory,
        format!("{} is not a folder", path.display()),
    )
}

/// How many links one resolution expands before it gives up, as the system does with `ELOOP`:
/// macOS gives up after 32 and Linux after 40, and the lower of the two is the one that holds
/// everywhere.
#[cfg(unix)]
const MAX_LINKS: u32 = 32;

/// One step of taking a path.
#[cfg(unix)]
enum Step {
    /// Start again at the root of the file system.
    Root,
    /// `..`: the parent of the folder reached so far.
    Up,
    /// A name.
    Name(OsString),
}

/// The pathname the system works on: the path as it was written and, in front of what is left of
/// it, the content of every link expanded since, the last one first. It is read from the front
/// one step at a time, and what is left of it, all the pieces together, is what the system counts
/// after the name of a link when it puts the content there.
#[cfg(unix)]
struct Pathname {
    /// The pieces, the one being read last.
    pieces: Vec<Piece>,
}

/// The text of a [`Pathname`] that is one path or the content of one link, and how much of it has
/// been read.
#[cfg(unix)]
struct Piece {
    text: Vec<u8>,
    /// How many bytes of `text` have been read.
    read: usize,
    /// Whether `text` is the path as it was written, and not the content of a link.
    written: bool,
}

#[cfg(unix)]
impl Pathname {
    fn new(path: &Path) -> Self {
        Self {
            pieces: vec![Piece {
                text: path.as_os_str().as_bytes().to_vec(),
                read: 0,
                written: true,
            }],
        }
    }

    /// Puts the content of a link in front of what is left.
    fn expand(&mut self, content: OsString) {
        self.pieces.push(Piece {
            text: content.into_vec(),
            read: 0,
            written: false,
        });
    }

    /// How many bytes are left to read, the separators that follow the last name included.
    fn left(&self) -> usize {
        self.pieces
            .iter()
            .map(|piece| piece.text.len() - piece.read)
            .sum()
    }

    /// The next step, and whether the path was written with it, or `None` when nothing is left. A
    /// separator that begins a piece, whether of the path or of the content of a link, is a step
    /// of its own: the root. Any other separator, and a name of `.`, is no step.
    fn next_step(&mut self) -> Option<(Step, bool)> {
        while let Some(piece) = self.pieces.last_mut() {
            let rest = &piece.text[piece.read..];
            let separators = rest.iter().take_while(|byte| **byte == b'/').count();
            let length = rest[separators..]
                .iter()
                .take_while(|byte| **byte != b'/')
                .count();
            let written = piece.written;
            if piece.read == 0 && separators > 0 {
                piece.read += separators;
                return Some((Step::Root, written));
            }
            if length == 0 {
                // Nothing, or nothing but separators, is left of this piece.
                self.pieces.pop();
                continue;
            }
            let name = &rest[separators..separators + length];
            let step = match name {
                b"." => None,
                b".." => Some(Step::Up),
                name => Some(Step::Name(OsStr::from_bytes(name).to_os_string())),
            };
            piece.read += separators + length;
            if let Some(step) = step {
                return Some((step, written));
            }
        }
        None
    }
}
