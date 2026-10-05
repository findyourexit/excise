//! The command line of `excise-shape`.
//!
//! ```text
//! excise-shape profile <ROOT> [--output FILE] [--cross-filesystems]
//! excise-shape spec <PROFILE> --id ID --entries N [--seed S] [--max-file-bytes BYTES] [--output FILE]
//! ```
//!
//! The binary is a few lines that call [`run`]; everything is here so that it can be tested
//! without starting a process. Nothing is printed that came from the tree: the summary on
//! standard error is counts, and no message names the root or anything below it.

use std::{
    error::Error,
    ffi::OsString,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{self, Read as _, Write},
    path::{Path, PathBuf},
    process::ExitCode,
};

use crate::{
    fixture::spec::{DEFAULT_MAX_FILE_BYTES, MAX_ENTRIES, is_identifier},
    report::{Document, HarnessShapeProfile},
};

use super::{
    derive::{SpecRequest, render_spec, spec_from_profile},
    walk::{WalkOptions, profile},
};

/// The seed of a spec when none is given.
pub const DEFAULT_SEED: u64 = 1;

/// The largest profile `spec` reads: a profile can come from anywhere, an issue for one, and a
/// real one is a few tens of kilobytes.
pub const MAX_PROFILE_BYTES: u64 = 16 * 1024 * 1024;

/// The text `excise-shape --help` prints.
pub const HELP: &str = "\
excise-shape: measure the shape of a tree, as aggregates only, and build a fixture specification
shaped like it.

USAGE:
    excise-shape profile <ROOT> [--output FILE] [--cross-filesystems]
    excise-shape spec <PROFILE> --id ID --entries N [--seed S] [--max-file-bytes BYTES] [--output FILE]
    excise-shape help [profile|spec]

COMMANDS:
    profile   Walk ROOT and write a profile of its shape: a JSON document of aggregates only.
    spec      Read a profile and write a fixture specification shaped like it, scaled to about
              N entries, for `cargo xtask headless --fixture-dir` and `cargo xtask bench-e2e
              --fixture-dir`.

Run `excise-shape help profile` or `excise-shape help spec` for the details of each.
";

/// The text `excise-shape help profile` prints.
pub const PROFILE_HELP: &str = "\
excise-shape profile: measure the shape of a tree.

USAGE:
    excise-shape profile <ROOT> [--output FILE] [--cross-filesystems]

Walks ROOT and writes a profile of its shape: a JSON document of aggregates only. It counts
entries by kind and by depth, and keeps histograms of what a folder holds, how large a file is
(in classes of powers of two), and how long a name is, with the share of hard links and symbolic
links. It holds no name, no path, no link target, no owner, and no timestamp, and none of them can
be rebuilt from it. No message of this command names ROOT or anything below it, and the message
for an output that exists, which can be below ROOT, names no path.

The walk only reads: it writes nothing, and --output makes its one new file after the walk has
ended, so a profile never counts its own output, wherever FILE is, ROOT included. It asks for the
metadata of each entry (`lstat`) and nothing else: it reads no file. The path of ROOT is resolved
like any path you type, and a link among its components is followed; its last component must be
a folder itself, not a link to one, and a `/` or a `.` after that name changes nothing (`link/` and
`link/.` are `link`, and a link is refused written either way). Nothing below ROOT is followed: a
symbolic link is an entry of its own, and the walk asks nothing of where it points, not even
whether its target exists. It stays on the file system of ROOT and counts the mount points it did
not enter, unless you pass --cross-filesystems.

It keeps no entry of the tree. At any moment it holds, in memory only (nothing goes to disk): the
names of the folder it is listing, all at once (a folder of a million files with names of 24
bytes took 73 MB on macOS, so the widest folder is what costs); the names of the subfolders it has
still to visit, in each folder on the path from ROOT to the folder it is in (roughly 100 bytes a
name); and the identity of every file that has more than one name, until the walk ends (roughly 60
bytes each). Its memory grows with those three and with nothing else of the tree: with the widest
folder, with the subfolders waiting along a path (in a chain of folders that each hold many
subfolders, they can approach the number of folders in the tree), and with the files that have
more than one name (at most every file). A tree for which any of them does not fit in memory
cannot be profiled. On Unix it keeps at most 32 folder handles open at once (a listing opens one
more while it reads), so that a tree of any depth stays within a descriptor limit of 256, and a
folder that is not the one it listed when it opens it is counted and not walked. On Windows it
holds the folders it is inside open, so that none can be renamed or deleted while it runs, and it
refuses a link or a junction where a folder was; it cannot tell one ordinary folder from another,
so a folder replaced by another ordinary folder before it is opened is walked. A folder it cannot
list is counted, and the walk goes on.

OPTIONS:
    --output FILE         Write the profile to FILE, which must not exist: a file is never
                          written over, and a link at its name is refused (the message names no
                          path, because FILE can be below ROOT). Folders above FILE that already
                          exist are taken as they are when the file is made, after the walk, links
                          included, as for any path you type; each one that is missing is made on
                          its own, so that a link that appears at its name is refused. The folders
                          it makes and the file are never links. On Unix the file is readable and
                          writable by its owner only (mode 0600), and the folders it makes are
                          0700; on Windows the file and those folders get the permissions of the
                          folder they are made in. Without this option the profile goes to
                          standard output.
    --cross-filesystems   Enter folders on other file systems too.

The profile describes your tree in aggregate, and it stays private to you until you share it:
keep it out of version control. This repository ignores `target/excise-profiles/`, which is the
place for it. A short summary of counts goes to standard error.
";

/// The text `excise-shape help spec` prints.
pub const SPEC_HELP: &str = "\
excise-shape spec: build a fixture specification from a profile.

USAGE:
    excise-shape spec <PROFILE> --id ID --entries N [--seed S] [--max-file-bytes BYTES] [--output FILE]

Reads a profile written by `excise-shape profile` and writes a fixture specification (TOML) that
the harness's fixture generator builds into a tree shaped like the profiled one, scaled to N
entries: a home of millions of entries at 50,000 for a quick run, or at 1,000,000 for a full one.
The spec plans exactly N entries, and the fixture holds one more, the ownership marker. The same
profile, N, and seed always give the same spec, and so the same fixture.

OPTIONS:
    --id ID               The id of the spec: 1 to 64 lowercase ASCII letters, digits, `-`, or `_`,
                          starting with a letter or digit. Save the spec as ID.toml.
    --entries N           How many entries to plan: at least one more than the levels the spec
                          keeps (the profile's depth, or 32 if it is deeper, because a spec holds
                          no more levels, and the root of the tree is one entry), and at most
                          10,100,000.
    --seed S              The seed of the spec, which decides which directory gets which share and
                          which file which size, not the shape. Default 1.
    --max-file-bytes B    The largest file to write, in bytes: a larger size is cut to it, because the
                          generator writes every byte. Default 16384, at most 1073741824.
    --output FILE         Write the spec to FILE, which must not exist, as `excise-shape profile
                          --output` writes its file (see `excise-shape help profile`). Without
                          it the spec goes to standard output.

Use the spec by keeping it in a directory of fixture specs of its own, named ID.toml, and running
    cargo xtask headless --fixture-dir DIR --fixture ID
    cargo xtask bench-e2e --baseline main --fixture-dir DIR --fixture ID

The spec holds aggregates only. Sockets, FIFOs, devices, folders that could not be listed, and
mount points are left out: a fixture cannot hold them.
";

/// A command line that does not say what to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

/// What a command line asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print a help text.
    Help(Help),
    /// Profile a tree.
    Profile {
        /// The root to walk.
        root: PathBuf,
        /// Where to write the profile; standard output when `None`.
        output: Option<PathBuf>,
        /// Whether to enter other file systems.
        cross_filesystems: bool,
    },
    /// Build a spec from a profile.
    Spec {
        /// The profile to read.
        profile: PathBuf,
        /// What to build.
        request: SpecRequest,
        /// Where to write the spec; standard output when `None`.
        output: Option<PathBuf>,
    },
}

/// Which help to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Help {
    /// The overview.
    General,
    /// `profile`.
    Profile,
    /// `spec`.
    Spec,
}

impl Help {
    /// The text.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::General => HELP,
            Self::Profile => PROFILE_HELP,
            Self::Spec => SPEC_HELP,
        }
    }
}

/// Reads a command line, without the program name.
///
/// # Errors
///
/// Returns what is wrong with it.
pub fn parse<I>(args: I) -> Result<Command, UsageError>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter();
    let Some(command) = args.next() else {
        return Err(UsageError("name a command: `profile` or `spec`".to_owned()));
    };
    match command.to_str() {
        Some("-h" | "--help") => Ok(Command::Help(Help::General)),
        Some("help") => match args.next().as_deref().and_then(std::ffi::OsStr::to_str) {
            None => Ok(Command::Help(Help::General)),
            Some("profile") => Ok(Command::Help(Help::Profile)),
            Some("spec") => Ok(Command::Help(Help::Spec)),
            Some(other) => Err(UsageError(format!(
                "there is no command `{other}`; the commands are `profile` and `spec`"
            ))),
        },
        Some("profile") => parse_profile(args),
        Some("spec") => parse_spec(args),
        _ => Err(UsageError(format!(
            "there is no command `{}`; the commands are `profile` and `spec`",
            command.to_string_lossy()
        ))),
    }
}

/// The value of the option `flag`, which must be the next argument.
fn value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString, UsageError> {
    args.next()
        .ok_or_else(|| UsageError(format!("`{flag}` needs a value")))
}

/// The text of a value, which must be UTF-8.
fn utf8(value: &OsString, flag: &str) -> Result<String, UsageError> {
    value
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| UsageError(format!("the value of `{flag}` is not text")))
}

/// A whole number.
fn number(value: &OsString, flag: &str) -> Result<u64, UsageError> {
    let given = utf8(value, flag)?;
    given
        .parse()
        .map_err(|_| UsageError(format!("`{flag}` takes a whole number, not `{given}`")))
}

fn parse_profile(mut args: impl Iterator<Item = OsString>) -> Result<Command, UsageError> {
    let mut root: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut cross_filesystems = false;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help(Help::Profile)),
            Some("--output") => {
                if output
                    .replace(value(&mut args, "--output")?.into())
                    .is_some()
                {
                    return Err(UsageError("`--output` is given twice".to_owned()));
                }
            }
            Some("--cross-filesystems") => cross_filesystems = true,
            Some(flag) if flag.starts_with('-') && flag != "-" => {
                return Err(UsageError(format!("unknown option `{flag}`")));
            }
            _ => {
                if root.replace(PathBuf::from(argument)).is_some() {
                    return Err(UsageError(
                        "name one root to profile, not several".to_owned(),
                    ));
                }
            }
        }
    }
    let root = root.ok_or_else(|| UsageError("name the root to profile".to_owned()))?;
    Ok(Command::Profile {
        root,
        output,
        cross_filesystems,
    })
}

fn parse_spec(mut args: impl Iterator<Item = OsString>) -> Result<Command, UsageError> {
    let mut profile: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut id: Option<String> = None;
    let mut entries: Option<u64> = None;
    let mut seed = DEFAULT_SEED;
    let mut max_file_bytes = DEFAULT_MAX_FILE_BYTES;
    while let Some(argument) = args.next() {
        match argument.to_str() {
            Some("-h" | "--help") => return Ok(Command::Help(Help::Spec)),
            Some("--output") => {
                if output
                    .replace(value(&mut args, "--output")?.into())
                    .is_some()
                {
                    return Err(UsageError("`--output` is given twice".to_owned()));
                }
            }
            Some("--id") => {
                let given = utf8(&value(&mut args, "--id")?, "--id")?;
                if !is_identifier(&given) {
                    return Err(UsageError(format!(
                        "`--id` takes 1 to 64 lowercase ASCII letters, digits, `-`, or `_`, \
                         starting with a letter or digit, not `{given}`"
                    )));
                }
                id = Some(given);
            }
            Some("--entries") => {
                entries = Some(number(&value(&mut args, "--entries")?, "--entries")?);
            }
            Some("--seed") => seed = number(&value(&mut args, "--seed")?, "--seed")?,
            Some("--max-file-bytes") => {
                max_file_bytes =
                    number(&value(&mut args, "--max-file-bytes")?, "--max-file-bytes")?;
            }
            Some(flag) if flag.starts_with('-') && flag != "-" => {
                return Err(UsageError(format!("unknown option `{flag}`")));
            }
            _ => {
                if profile.replace(PathBuf::from(argument)).is_some() {
                    return Err(UsageError(
                        "name one profile to read, not several".to_owned(),
                    ));
                }
            }
        }
    }
    let profile = profile.ok_or_else(|| UsageError("name the profile to read".to_owned()))?;
    let id = id.ok_or_else(|| UsageError("`--id` is needed".to_owned()))?;
    let entries = entries.ok_or_else(|| UsageError("`--entries` is needed".to_owned()))?;
    if entries == 0 || entries > MAX_ENTRIES {
        return Err(UsageError(format!(
            "`--entries` takes 1 to {MAX_ENTRIES}, not {entries}"
        )));
    }
    Ok(Command::Spec {
        profile,
        request: SpecRequest {
            id,
            entries,
            seed,
            max_file_bytes,
        },
        output,
    })
}

/// Runs a command line, without the program name, and returns the exit status: [`SUCCESS`] when
/// it worked, [`FAILURE`] when it failed, [`USAGE`] when the command line was wrong.
pub fn run<I>(args: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode
where
    I: IntoIterator<Item = OsString>,
{
    ExitCode::from(status(args, stdout, stderr))
}

/// The exit status of a run that worked.
pub const SUCCESS: u8 = 0;
/// The exit status of a run that failed.
pub const FAILURE: u8 = 1;
/// The exit status of a command line that was wrong.
pub const USAGE: u8 = 2;

/// [`run`], as the number the process exits with.
pub fn status<I>(args: I, stdout: &mut dyn Write, stderr: &mut dyn Write) -> u8
where
    I: IntoIterator<Item = OsString>,
{
    let command = match parse(args) {
        Ok(command) => command,
        Err(UsageError(message)) => {
            let _ = writeln!(
                stderr,
                "excise-shape: {message}\nTry `excise-shape --help`."
            );
            return USAGE;
        }
    };
    let outcome = match command {
        Command::Help(help) => stdout.write_all(help.text().as_bytes()).map_err(Into::into),
        Command::Profile {
            root,
            output,
            cross_filesystems,
        } => run_profile(&root, output.as_deref(), cross_filesystems, stdout, stderr),
        Command::Spec {
            profile,
            request,
            output,
        } => run_spec(&profile, &request, output.as_deref(), stdout, stderr),
    };
    match outcome {
        Ok(()) => SUCCESS,
        Err(error) => {
            let _ = writeln!(stderr, "excise-shape: {error}");
            FAILURE
        }
    }
}

fn run_profile(
    root: &Path,
    output: Option<&Path>,
    cross_filesystems: bool,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), Box<dyn Error>> {
    // Before the walk, which can take minutes: a profile is never written over a file.
    if let Some(path) = output {
        refuse_existing(path, Output::Profile)?;
    }
    let shape = profile(root, WalkOptions { cross_filesystems })?;
    if let Err(problems) = shape.check() {
        return Err(format!("internal error: the profile breaks its own rules: {problems}").into());
    }
    deliver(&shape.to_json_pretty()?, output, stdout)?;
    writeln!(stderr, "excise-shape: profiled {}", summary(&shape))?;
    Ok(())
}

fn run_spec(
    profile_path: &Path,
    request: &SpecRequest,
    output: Option<&Path>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), Box<dyn Error>> {
    if let Some(path) = output {
        refuse_existing(path, Output::Spec)?;
    }
    let text = read_bounded(profile_path)?;
    let shape = HarnessShapeProfile::from_json_str(&text).map_err(|error| {
        format!(
            "`{}` is not a shape profile of this version: {error}",
            profile_path.display()
        )
    })?;
    let spec = spec_from_profile(&shape, request)?;
    deliver(&render_spec(&spec)?, output, stdout)?;
    writeln!(
        stderr,
        "excise-shape: `{}` plans {} entries in {} levels, shaped like a profile of {} entries",
        spec.id,
        spec.planned_entry_count(),
        shape.max_depth.min(crate::fixture::spec::MAX_SHAPED_DEPTH),
        shape.entries.total
    )?;
    Ok(())
}

/// The counts of a profile, as a line.
fn summary(shape: &HarnessShapeProfile) -> String {
    let entries = &shape.entries;
    let line = format!(
        "{} entries ({} folders, {} files, {} symbolic links, {} others) to a depth of {}",
        entries.total,
        entries.directories,
        entries.files,
        entries.symlinks,
        entries.others,
        shape.max_depth
    );
    let notes: Vec<String> = [
        (
            shape.unreadable.directories,
            "folders that could not be listed",
        ),
        (shape.unreadable.errors, "entries that could not be read"),
        (shape.walk.mount_points_skipped, "mount points not entered"),
    ]
    .into_iter()
    .filter(|(count, _)| *count > 0)
    .map(|(count, what)| format!("{count} {what}"))
    .collect();
    if notes.is_empty() {
        line
    } else {
        format!("{line}; {}", notes.join("; "))
    }
}

/// What a message about an output that exists may say.
#[derive(Debug, Clone, Copy)]
enum Output {
    /// The file of a profile. It can be inside the root that was walked, so no message about it
    /// names a path: no message of `profile` names the root or anything below it.
    Profile,
    /// The file of a spec. A message about it may name it: nothing in a spec's run comes from a
    /// tree that was profiled.
    Spec,
}

/// Fails when `path` is anything, a link included: the file it names is never written over, and a
/// link at its name is never followed. What lies above it is not looked at here.
fn refuse_existing(path: &Path, output: Output) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            match output {
                Output::Profile => {
                    "the output file already exists; a profile never replaces a file".to_owned()
                }
                Output::Spec => format!(
                    "`{}` exists: remove it or name another file",
                    path.display()
                ),
            },
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Reads a file of at most [`MAX_PROFILE_BYTES`] as text.
fn read_bounded(path: &Path) -> io::Result<String> {
    let mut text = String::new();
    File::open(path)?
        .take(MAX_PROFILE_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_PROFILE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "`{}` is larger than {MAX_PROFILE_BYTES} bytes, which no profile is",
                path.display()
            ),
        ));
    }
    Ok(text)
}

/// Writes `text` to `output`, or to `stdout`.
fn deliver(text: &str, output: Option<&Path>, stdout: &mut dyn Write) -> io::Result<()> {
    match output {
        Some(path) => write_new_file(path, text),
        None => stdout.write_all(text.as_bytes()),
    }
}

/// Creates `path`, which must not exist, and writes `text` to it.
///
/// On Unix the file is readable and writable by its owner only (mode 0600), and a folder this
/// makes above it is 0700. On Windows the file and those folders get the permissions of the
/// folder they are made in: an owner-only ACL needs `unsafe` code or a Windows security
/// dependency, which this crate has neither of, and a profile is private because of what it
/// holds, not because of its permissions.
///
/// Folders above `path` that already exist are taken as they are *when the file is made*: a link
/// among them is followed, as for any path a person types, and so is one that replaces such a
/// folder between the moment this looks at the path and the moment it writes, because the path is
/// not held open. What is never a link is what this makes: every folder that is missing is made
/// on its own, one at a time and never recursively, so anything that appears at its name first, a
/// link included, is refused, and the file is created new, which refuses a link at its name too.
/// Nothing is written over a file.
///
/// # Errors
///
/// Returns why a folder or the file could not be made, or the file written.
pub fn write_new_file(path: &Path, text: &str) -> io::Result<()> {
    create_new_file(path, text, &mut |_| {})
}

/// [`write_new_file`], calling `before_make` with each missing folder just before it is made: a
/// test puts something at its name there.
pub(super) fn create_new_file(
    path: &Path,
    text: &str,
    before_make: &mut dyn FnMut(&Path),
) -> io::Result<()> {
    // The folders above `path` that are missing, deepest first: the first one that exists, links
    // followed, ends the list.
    let mut missing = Vec::new();
    let mut folder = path.parent();
    while let Some(candidate) =
        folder.filter(|candidate| !candidate.as_os_str().is_empty() && !candidate.is_dir())
    {
        missing.push(candidate);
        folder = candidate.parent();
    }
    for folder in missing.into_iter().rev() {
        before_make(folder);
        make_folder(folder)?;
    }
    let mut options = OpenOptions::new();
    // `create_new` refuses a path that exists, a link included, and never follows one.
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(text.as_bytes())
}

/// Makes the folder `path`, which must not exist: not recursively, so that anything at its name
/// is refused.
fn make_folder(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        DirBuilder::new().mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        DirBuilder::new().create(path)
    }
}
