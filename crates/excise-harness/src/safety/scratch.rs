//! The per-run scratch area and its exact residue check.

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use tempfile::TempDir;
use thiserror::Error;

/// The mode of the root of an area and of every directory in it: its owner's alone.
#[cfg(unix)]
const PRIVATE_DIRECTORY: u32 = 0o700;

/// The mode of the configuration file of an area: its owner may read and write it.
#[cfg(unix)]
const PRIVATE_FILE: u32 = 0o600;

/// The content of the scratch configuration file: an empty configuration in the only supported
/// version. `EXCISE_CONFIG` must name an existing file, and an unset variable would fall back to
/// the user's real configuration directory.
const EMPTY_CONFIG: &str = "version = 1\n";

/// A scratch area could not be created or inspected.
#[derive(Debug, Error)]
#[error("scratch area `{}`: {source}", path.display())]
pub struct ScratchError {
    path: PathBuf,
    source: io::Error,
}

impl ScratchError {
    fn new(path: &Path, source: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The directories one `excise` run is allowed to touch besides its fixture.
///
/// ```text
/// <root>/
///   home/               HOME: nothing may be written here
///   config/config.toml  EXCISE_CONFIG: an empty configuration; a theme commit may rewrite it
///   cwd/                the working directory: exports land here, so nothing may be
///   store/              EXCISE_SCAN_STORE_DIR: nothing may be left here after an exit
///   tmp/                TMPDIR: nothing may be left here
///   events.jsonl        EXCISE_TEST_EVENTS: created by `excise`, never by the harness
///   scan-report.json    `--output` of a headless run: created by `excise`, never by the harness
/// ```
///
/// Because `excise` is given no other writable location, [`Scratch::residue`] is an exact check:
/// anything it reports was left behind by the run. [`Scratch::visit_residue`] is the same check
/// under a bound, for a caller that cannot wait for a walk of whatever the run left. Dropping a
/// `Scratch` deletes the tree unless [`Scratch::keep`] was called. The area is private to its
/// owner on Unix, whatever the umask is ([`Scratch::create`]).
#[derive(Debug)]
pub struct Scratch {
    dir: Option<TempDir>,
    root: PathBuf,
    kept: bool,
}

impl Scratch {
    /// Creates a scratch area in a new directory below `parent`, private to its owner: the root
    /// and the directories in it have the mode `0700` and the configuration file `0600`, whatever
    /// the umask of the process is (Unix). The area holds what a run writes, and a scan's report
    /// names every entry of the tree it scanned, so nobody else may read it, or put a file or a
    /// link in it for the program to open. The mode that the system gives a new directory is the
    /// umask's, which is `0755` under the usual one and can leave others a way in, or take a
    /// bit from the owner, so every mode is set on what was made, exactly.
    ///
    /// # Errors
    ///
    /// Returns an error if the directories or the configuration file cannot be created.
    pub fn create(parent: &Path) -> Result<Self, ScratchError> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("xh-scratch-");
        #[cfg(unix)]
        builder.permissions(fs::Permissions::from_mode(PRIVATE_DIRECTORY));
        let dir = builder
            .tempdir_in(parent)
            .map_err(|source| ScratchError::new(parent, source))?;
        let root = dir.path().to_path_buf();
        let scratch = Self {
            dir: Some(dir),
            root,
            kept: false,
        };
        // What the umask took from the owner is given back, so that the area can be made in.
        #[cfg(unix)]
        fs::set_permissions(&scratch.root, fs::Permissions::from_mode(PRIVATE_DIRECTORY))
            .map_err(|source| ScratchError::new(&scratch.root, source))?;
        for directory in [
            scratch.home(),
            scratch.config_dir(),
            scratch.cwd(),
            scratch.store(),
            scratch.tmp(),
        ] {
            make_directory(&directory).map_err(|source| ScratchError::new(&directory, source))?;
        }
        let config = scratch.config_file();
        write_private_file(&config, EMPTY_CONFIG)
            .map_err(|source| ScratchError::new(&config, source))?;
        Ok(scratch)
    }

    /// The scratch root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `HOME` directory.
    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// The directory holding the configuration file.
    #[must_use]
    pub fn config_dir(&self) -> PathBuf {
        self.root.join("config")
    }

    /// The `EXCISE_CONFIG` file.
    #[must_use]
    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    /// The working directory of the run.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.root.join("cwd")
    }

    /// The `EXCISE_SCAN_STORE_DIR` directory.
    #[must_use]
    pub fn store(&self) -> PathBuf {
        self.root.join("store")
    }

    /// The temporary directory of the run.
    #[must_use]
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    /// The `EXCISE_TEST_EVENTS` file. It does not exist until `excise` creates it.
    #[must_use]
    pub fn events(&self) -> PathBuf {
        self.root.join("events.jsonl")
    }

    /// The `--output` file of a headless run. It does not exist until `excise` creates it.
    #[must_use]
    pub fn report(&self) -> PathBuf {
        self.root.join("scan-report.json")
    }

    /// Keeps the scratch area on disk when the value is dropped, and returns its root.
    pub fn keep(&mut self) -> PathBuf {
        self.kept = true;
        if let Some(dir) = self.dir.take() {
            let _ = dir.keep();
        }
        self.root.clone()
    }

    /// Whether [`Scratch::keep`] was called.
    #[must_use]
    pub const fn is_kept(&self) -> bool {
        self.kept
    }

    /// Removes the area now, with everything in it, and says why when it cannot. Dropping a
    /// `Scratch` removes the area too, but a failure to remove goes unseen there: a caller for
    /// which a leftover area is a finding of its own calls this instead. An area that
    /// [`Scratch::keep`] kept is not removed: it was asked to stay, and that is not an error.
    ///
    /// # Errors
    ///
    /// Returns an error when the area could not be removed completely, for instance because the
    /// run left a directory in it that its owner cannot write to. What could not be removed is
    /// still there.
    pub fn close(mut self) -> Result<(), ScratchError> {
        match self.dir.take() {
            Some(dir) => dir
                .close()
                .map_err(|source| ScratchError::new(&self.root, source)),
            None => Ok(()),
        }
    }

    /// Everything in the scratch area that the run should not have left behind, as paths relative
    /// to the root.
    ///
    /// The allowed remainder is the layout in the type documentation: the configuration file, the
    /// event file and the report file, and the five directories, with `home/`, `cwd/`, `store/`,
    /// and `tmp/` empty. A directory that holds unexpected entries is reported through those
    /// entries. Call this after the process has exited.
    ///
    /// # Errors
    ///
    /// Returns an error if the area cannot be read.
    pub fn residue(&self) -> Result<Vec<String>, ScratchError> {
        let mut found = Vec::new();
        for entry in sorted_entries(&self.root)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|source| ScratchError::new(&path, source))?;
            match name.as_str() {
                "home" | "cwd" | "store" | "tmp" if kind.is_dir() => {
                    collect_children(&path, &name, &mut found)?;
                }
                "config" if kind.is_dir() => {
                    for child in sorted_entries(&path)? {
                        let child_name = child.file_name().to_string_lossy().into_owned();
                        let child_kind = child
                            .file_type()
                            .map_err(|source| ScratchError::new(&child.path(), source))?;
                        if child_name != "config.toml" || !child_kind.is_file() {
                            collect_entry(
                                &child.path(),
                                &format!("{name}/{child_name}"),
                                &mut found,
                            )?;
                        }
                    }
                }
                "events.jsonl" | "scan-report.json" if kind.is_file() => {}
                _ => collect_entry(&path, &name, &mut found)?,
            }
        }
        Ok(found)
    }

    /// [`Scratch::residue`] under a bound, for a caller that cannot let the program it ran decide
    /// how long the look takes or how much it holds: a program that left a vast hierarchy behind
    /// must not keep the caller from ending, and a caller that is being stopped must not wait for
    /// a walk.
    ///
    /// `found` is called with the path, relative to the root, of every entry that
    /// [`Scratch::residue`] would list, in the order it lists them (sorted by name at every level,
    /// a directory before what is in it), and keeps nothing: what to hold is the caller's to
    /// decide. At most `max_entries` entries of the area are read, the expected ones included,
    /// and `stop` is asked before each one is read and before each one is gone through. The result
    /// is `true` when the look reached the end of the area. It is `false` when the look was cut
    /// short, by the limit or by `stop`, and then `found` was given the start of the residue and
    /// not all of it. A directory with more entries than are left to read is read as far as that
    /// goes and no further, so that its size is never held. After a cut by the limit the entries
    /// that were read are still gone through; after a cut by `stop` they are not.
    ///
    /// One entry is treated otherwise than [`Scratch::residue`] treats it: `scan-report.json` is
    /// expected whatever it is, and is never entered. The report is the caller's to read. A
    /// program that left a folder there, a link, or something else made a report that cannot be
    /// read, which the reader says without anyone walking what a folder holds. Everything else is
    /// as it is there, an error included: when a directory of the area cannot be read, nothing is
    /// made up for what could not be listed.
    ///
    /// # Errors
    ///
    /// Returns an error if a directory of the area cannot be read.
    pub fn visit_residue(
        &self,
        max_entries: usize,
        stop: &mut dyn FnMut() -> bool,
        found: &mut dyn FnMut(&str),
    ) -> Result<bool, ScratchError> {
        let mut look = Look {
            left: max_entries,
            complete: true,
            stop,
        };
        let mut levels = vec![Level {
            place: Place::Root,
            relative: String::new(),
            entries: look.read(&self.root)?,
        }];
        while let Some(top) = levels.len().checked_sub(1) {
            if look.stopped() {
                return Ok(false);
            }
            let Some(entry) = levels[top].entries.next() else {
                levels.pop();
                continue;
            };
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            let kind = entry
                .file_type()
                .map_err(|source| ScratchError::new(&path, source))?;
            let relative = if levels[top].relative.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", levels[top].relative)
            };
            // What the entry is: expected, a directory that holds expected entries, or residue,
            // which is reported and, for a directory, gone into. `descend` is where the directory
            // goes into, if it does.
            let descend = match (levels[top].place, name.as_str()) {
                (Place::Root, "home" | "cwd" | "store" | "tmp") if kind.is_dir() => {
                    Some(Place::Residue)
                }
                (Place::Root, "config") if kind.is_dir() => Some(Place::Config),
                // The report is the caller's to read, whatever it is.
                (Place::Root, "scan-report.json") => None,
                (Place::Root, "events.jsonl") | (Place::Config, "config.toml")
                    if kind.is_file() =>
                {
                    None
                }
                _ => {
                    found(&relative);
                    kind.is_dir().then_some(Place::Residue)
                }
            };
            if let Some(place) = descend {
                levels.push(Level {
                    place,
                    entries: look.read(&path)?,
                    relative,
                });
            }
        }
        Ok(look.complete)
    }
}

/// Makes the directory at `path`, which is new. On Unix it is private to its owner whatever the
/// umask is: the mode that a directory is made with goes through the umask, so the mode is set
/// exactly once the directory is there. Elsewhere it is an ordinary directory.
fn make_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::DirBuilder::new().mode(PRIVATE_DIRECTORY).create(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY))
    }
    #[cfg(not(unix))]
    {
        fs::create_dir(path)
    }
}

/// Writes `content` to a new file at `path`. On Unix its owner alone can read and write it
/// whatever the umask is: the mode that a file is made with goes through the umask, so the mode is
/// set exactly, on the open file, before the content is written.
fn write_private_file(path: &Path, content: &str) -> io::Result<()> {
    use io::Write as _;

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(PRIVATE_FILE);
    let mut file = options.open(path)?;
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE))?;
    file.write_all(content.as_bytes())
}

fn sorted_entries(directory: &Path) -> Result<Vec<fs::DirEntry>, ScratchError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| ScratchError::new(directory, source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| ScratchError::new(directory, source))?;
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

/// Reports every entry below `directory`, but not `directory` itself.
fn collect_children(
    directory: &Path,
    relative: &str,
    found: &mut Vec<String>,
) -> Result<(), ScratchError> {
    for entry in sorted_entries(directory)? {
        let name = entry.file_name().to_string_lossy().into_owned();
        collect_entry(&entry.path(), &format!("{relative}/{name}"), found)?;
    }
    Ok(())
}

/// Reports `path` and, for a real directory, everything below it. Symbolic links are reported and
/// never followed.
fn collect_entry(path: &Path, relative: &str, found: &mut Vec<String>) -> Result<(), ScratchError> {
    found.push(relative.to_owned());
    let metadata = fs::symlink_metadata(path).map_err(|source| ScratchError::new(path, source))?;
    if metadata.is_dir() {
        collect_children(path, relative, found)?;
    }
    Ok(())
}

/// Where the entries of a directory that a look goes through stand in the layout of the area,
/// which says which of them are residue.
#[derive(Clone, Copy)]
enum Place {
    /// The root of the area: its layout says which entries are expected.
    Root,
    /// `config/`: only `config.toml` is expected.
    Config,
    /// Everything in it is residue: `home/`, `cwd/`, `store/`, and `tmp/`, and every directory
    /// that is residue itself.
    Residue,
}

/// A directory whose entries a look has read and is going through.
struct Level {
    place: Place,
    /// Its path relative to the root of the area, which is empty for the root.
    relative: String,
    entries: std::vec::IntoIter<fs::DirEntry>,
}

/// What a bounded look has left to spend, and whether it has had to leave something unread.
struct Look<'a> {
    /// How many entries it may still read.
    left: usize,
    /// Whether every entry it has met was read.
    complete: bool,
    stop: &'a mut dyn FnMut() -> bool,
}

impl Look<'_> {
    /// Whether the look must end now. It is not complete after that.
    fn stopped(&mut self) -> bool {
        let stopped = (self.stop)();
        if stopped {
            self.complete = false;
        }
        stopped
    }

    /// The entries of `directory`, sorted by name, as many as the look may still read. An entry
    /// that is not read, because the limit is spent or the look was stopped, makes the look
    /// incomplete.
    fn read(&mut self, directory: &Path) -> Result<std::vec::IntoIter<fs::DirEntry>, ScratchError> {
        let listing =
            fs::read_dir(directory).map_err(|source| ScratchError::new(directory, source))?;
        let mut entries = Vec::new();
        for entry in listing {
            if self.left == 0 || (self.stop)() {
                self.complete = false;
                break;
            }
            entries.push(entry.map_err(|source| ScratchError::new(directory, source))?);
            self.left -= 1;
        }
        entries.sort_by_cached_key(fs::DirEntry::file_name);
        Ok(entries.into_iter())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn scratch() -> Scratch {
        Scratch::create(&std::env::temp_dir()).expect("a scratch area")
    }

    #[test]
    fn a_fresh_area_has_no_residue() {
        let scratch = scratch();

        assert_eq!(scratch.residue().expect("residue"), Vec::<String>::new());
    }

    #[test]
    fn the_configuration_file_names_a_supported_version() {
        let scratch = scratch();

        assert_eq!(
            fs::read_to_string(scratch.config_file()).expect("config"),
            EMPTY_CONFIG
        );
    }

    #[test]
    fn the_event_file_the_report_and_a_rewritten_configuration_are_not_residue() {
        let scratch = scratch();
        fs::write(scratch.events(), b"{}\n").expect("events");
        fs::write(scratch.report(), b"{}\n").expect("report");
        fs::write(scratch.config_file(), "version = 1\n[runtime]\n").expect("config");

        assert_eq!(scratch.residue().expect("residue"), Vec::<String>::new());
    }

    #[test]
    fn a_directory_in_the_place_of_the_report_is_reported_with_its_contents() {
        let scratch = scratch();
        fs::create_dir(scratch.report()).expect("a directory");
        fs::write(scratch.report().join("inner"), b"x").expect("inner file");

        assert_eq!(
            scratch.residue().expect("residue"),
            ["scan-report.json", "scan-report.json/inner"]
        );
    }

    #[test]
    fn a_session_directory_left_in_the_store_is_reported_with_its_files() {
        let scratch = scratch();
        let session = scratch.store().join(".excise-scan-AbC123");
        fs::create_dir_all(session.join("runs")).expect("session");
        fs::write(session.join("runs/run-0"), b"x").expect("run file");

        assert_eq!(
            scratch.residue().expect("residue"),
            [
                "store/.excise-scan-AbC123",
                "store/.excise-scan-AbC123/runs",
                "store/.excise-scan-AbC123/runs/run-0",
            ]
        );
    }

    #[test]
    fn files_left_in_the_working_directory_home_and_tmp_are_reported() {
        let scratch = scratch();
        fs::write(scratch.cwd().join("export.json"), b"{}").expect("export");
        fs::write(scratch.home().join(".excise"), b"x").expect("home file");
        fs::write(scratch.tmp().join("spill"), b"x").expect("spill");

        assert_eq!(
            scratch.residue().expect("residue"),
            ["cwd/export.json", "home/.excise", "tmp/spill"]
        );
    }

    #[test]
    fn stray_entries_anywhere_else_are_reported() {
        let scratch = scratch();
        fs::write(scratch.root().join("stray"), b"x").expect("stray");
        fs::write(scratch.config_dir().join("second.toml"), b"x").expect("second config");

        assert_eq!(
            scratch.residue().expect("residue"),
            ["config/second.toml", "stray"]
        );
    }

    #[test]
    fn a_replaced_directory_is_reported() {
        let scratch = scratch();
        fs::remove_dir(scratch.store()).expect("remove the store");
        fs::write(scratch.store(), b"not a directory").expect("a file in its place");

        assert_eq!(scratch.residue().expect("residue"), ["store"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_symbolic_link_is_reported_and_not_followed() {
        let scratch = scratch();
        let outside = tempfile::tempdir().expect("an outside directory");
        fs::write(outside.path().join("file"), b"x").expect("a file outside");
        std::os::unix::fs::symlink(outside.path(), scratch.store().join("link")).expect("a link");

        assert_eq!(scratch.residue().expect("residue"), ["store/link"]);
    }

    /// What a look at `scratch` finds that is given `max_entries` and is never stopped, and
    /// whether it reached the end of the area.
    fn look(scratch: &Scratch, max_entries: usize) -> (Vec<String>, bool) {
        let mut names = Vec::new();
        let complete = scratch
            .visit_residue(max_entries, &mut || false, &mut |name| {
                names.push(name.to_owned());
            })
            .expect("a look");
        (names, complete)
    }

    /// How many entries a look reads in an area that holds nothing but what is expected: the
    /// least that lets it reach the end.
    fn entries_of_a_fresh_area() -> usize {
        let area = scratch();
        (0..100)
            .find(|&entries| look(&area, entries).1)
            .expect("a fresh area is small")
    }

    /// Makes a directory that a test locked usable again when it is dropped, so that the area it
    /// is in can be removed even when the test fails with the mode in place.
    #[cfg(unix)]
    struct Unlock(PathBuf);

    #[cfg(unix)]
    impl Drop for Unlock {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt as _;

            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700));
        }
    }

    /// Locks `directory`, and says whether it is locked for this process: one that is root can
    /// read every folder.
    #[cfg(unix)]
    fn lock(directory: &Path) -> bool {
        use std::os::unix::fs::PermissionsExt as _;

        fs::set_permissions(directory, fs::Permissions::from_mode(0o000)).expect("a mode");
        fs::read_dir(directory).is_err()
    }

    #[test]
    fn a_look_that_nothing_cuts_lists_what_residue_lists_in_the_same_order() {
        let scratch = scratch();
        fs::write(scratch.cwd().join("export.json"), b"{}").expect("export");
        fs::write(scratch.home().join(".excise"), b"x").expect("home file");
        fs::write(scratch.tmp().join("spill"), b"x").expect("spill");
        let session = scratch.store().join(".excise-scan-AbC123");
        fs::create_dir_all(session.join("runs")).expect("session");
        fs::write(session.join("runs/run-1"), b"x").expect("run file");
        fs::write(session.join("runs/run-0"), b"x").expect("run file");
        fs::create_dir_all(scratch.root().join("zz-folder/inner")).expect("a stray folder");
        fs::write(scratch.root().join("zz-folder/inner/file"), b"x").expect("a file in it");
        fs::write(scratch.root().join("stray"), b"x").expect("stray");
        fs::write(scratch.config_dir().join("second.toml"), b"x").expect("second config");
        fs::write(scratch.events(), b"{}\n").expect("events");

        let (names, complete) = look(&scratch, usize::MAX);

        assert!(complete);
        assert_eq!(names, scratch.residue().expect("residue"));
        assert_eq!(names.len(), 12, "{names:?}");
    }

    #[test]
    fn a_report_that_is_a_folder_is_expected_and_is_not_entered() {
        let area = scratch();
        let fresh = entries_of_a_fresh_area();
        fs::create_dir_all(area.report().join("deeper")).expect("a folder in the report's place");
        fs::write(area.report().join("deeper/inner"), b"x").expect("a file in it");

        // The report's own entry is one more entry to read, and none of what the folder holds.
        assert_eq!(look(&area, fresh + 1), (Vec::new(), true));
        // The exact check lists it, as it does anything that is not a regular file there.
        assert_eq!(
            area.residue().expect("residue"),
            [
                "scan-report.json",
                "scan-report.json/deeper",
                "scan-report.json/deeper/inner"
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_report_folder_that_holds_what_cannot_be_read_is_not_an_error() {
        let area = scratch();
        let locked = area.report().join("locked");
        fs::create_dir_all(&locked).expect("a folder in the report's place");
        let _unlock = Unlock(locked.clone());
        if !lock(&locked) {
            eprintln!("skipped: a process that is root can read every folder");
            return;
        }

        assert_eq!(look(&area, usize::MAX), (Vec::new(), true));
        // The same folder anywhere else in the area is an area that cannot be read.
        let elsewhere = area.cwd().join("locked");
        fs::create_dir(&elsewhere).expect("a folder in the working directory");
        let _unlock_it = Unlock(elsewhere.clone());
        assert!(lock(&elsewhere));
        let outcome = area.visit_residue(usize::MAX, &mut || false, &mut |_| {});
        assert!(outcome.is_err(), "{outcome:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_report_that_is_a_link_is_expected_and_is_not_followed() {
        let area = scratch();
        let outside = tempfile::tempdir().expect("an outside directory");
        fs::write(outside.path().join("file"), b"x").expect("a file outside");
        std::os::unix::fs::symlink(outside.path(), area.report()).expect("a link");

        assert_eq!(look(&area, usize::MAX), (Vec::new(), true));
    }

    #[test]
    fn a_look_reads_no_more_entries_than_it_is_given_and_says_when_it_stopped_short() {
        let area = scratch();
        let fresh = entries_of_a_fresh_area();
        for index in 0..50 {
            fs::write(area.cwd().join(format!("file-{index:02}")), b"x").expect("a file");
        }

        let (names, complete) = look(&area, fresh + 50);
        assert!(complete);
        assert_eq!(names.len(), 50);
        assert_eq!(names.first().map(String::as_str), Some("cwd/file-00"));
        assert_eq!(names.last().map(String::as_str), Some("cwd/file-49"));

        for allowed in [0, 1, 10, 49] {
            let (names, complete) = look(&area, fresh + allowed);
            assert!(!complete, "{allowed}");
            assert_eq!(names.len(), allowed, "{allowed}");
            // What it read is in order, though not the first of the directory: a directory is
            // read in the order the file system gives it, and sorted after.
            assert!(names.is_sorted(), "{names:?}");
        }
        let (names, complete) = look(&area, 0);
        assert!(!complete && names.is_empty());
    }

    #[test]
    fn a_directory_with_more_entries_than_the_look_may_read_is_not_read_to_its_end() {
        let area = scratch();
        for index in 0..3_000 {
            fs::write(area.tmp().join(format!("spill-{index:04}")), b"").expect("a file");
        }
        let asked = Cell::new(0_u32);
        let mut found = 0_usize;

        let complete = area
            .visit_residue(
                100,
                &mut || {
                    asked.set(asked.get() + 1);
                    false
                },
                &mut |_| found += 1,
            )
            .expect("a look");

        assert!(!complete);
        assert!(found <= 100, "{found}");
        assert!(
            asked.get() < 400,
            "the look asked whether to stop {} times: it read the directory to its end",
            asked.get()
        );
    }

    #[test]
    fn a_look_that_is_told_to_stop_ends_at_once_and_says_so() {
        let area = scratch();
        for index in 0..30 {
            fs::write(area.cwd().join(format!("file-{index:02}")), b"x").expect("a file");
        }

        let asked = Cell::new(0_u32);
        let mut names = Vec::new();
        let complete = area
            .visit_residue(
                usize::MAX,
                &mut || {
                    asked.set(asked.get() + 1);
                    true
                },
                &mut |name| names.push(name.to_owned()),
            )
            .expect("a look");
        assert!(!complete);
        assert!(names.is_empty(), "{names:?}");
        assert!(asked.get() <= 2, "it asked at once, and was told to stop");

        // Stopped while it goes through what it read: it has found the start of the residue.
        let asked = Cell::new(0_u32);
        let mut names = Vec::new();
        let complete = area
            .visit_residue(
                usize::MAX,
                &mut || {
                    asked.set(asked.get() + 1);
                    asked.get() > 50
                },
                &mut |name| names.push(name.to_owned()),
            )
            .expect("a look");
        assert!(!complete);
        assert!(
            !names.is_empty() && names.len() < 30,
            "the start of the residue, and not all of it: {names:?}"
        );
        assert!(
            asked.get() <= 60,
            "it asked again after it was told to stop: {}",
            asked.get()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_of_the_area_that_cannot_be_read_is_an_error_and_not_residue() {
        let area = scratch();
        let _unlock = Unlock(area.home());
        if !lock(&area.home()) {
            eprintln!("skipped: a process that is root can read every folder");
            return;
        }
        let mut names = Vec::new();

        let outcome = area.visit_residue(usize::MAX, &mut || false, &mut |name| {
            names.push(name.to_owned());
        });

        assert!(outcome.is_err(), "{outcome:?}");
        assert!(names.is_empty(), "nothing is made up for what was not read");
    }

    #[test]
    fn dropping_the_area_removes_it_and_keeping_it_does_not() {
        let dropped = scratch();
        let dropped_root = dropped.root().to_path_buf();
        drop(dropped);
        assert!(!dropped_root.exists());

        let mut kept = scratch();
        let kept_root = kept.keep();
        assert!(kept.is_kept());
        drop(kept);
        assert!(kept_root.exists());
        fs::remove_dir_all(kept_root).expect("clean up the kept area");
    }

    #[test]
    fn closing_the_area_removes_it_with_what_a_run_left_in_it_and_a_kept_area_stays() {
        let closed = scratch();
        let closed_root = closed.root().to_path_buf();
        fs::write(closed.tmp().join("left-by-a-compiler"), b"x").expect("a file");

        closed.close().expect("the area is removed");
        assert!(!closed_root.exists());

        let mut kept = scratch();
        let kept_root = kept.keep();
        kept.close()
            .expect("a kept area is not removed, and that is not an error");
        assert!(kept_root.exists());
        fs::remove_dir_all(kept_root).expect("clean up the kept area");
    }

    #[cfg(unix)]
    #[test]
    fn an_area_that_cannot_be_removed_is_an_error_that_names_it_and_the_area_stays() {
        use std::os::unix::fs::PermissionsExt as _;

        // A process that is root can remove what it likes, and there is nothing to try.
        let probe = tempfile::tempdir().expect("a directory");
        let locked_probe = probe.path().join("locked");
        fs::create_dir(&locked_probe).expect("a directory");
        fs::write(locked_probe.join("file"), b"x").expect("a file");
        fs::set_permissions(&locked_probe, fs::Permissions::from_mode(0o500)).expect("chmod");
        let removable = fs::remove_file(locked_probe.join("file")).is_ok();
        fs::set_permissions(&locked_probe, fs::Permissions::from_mode(0o700)).expect("chmod");
        if removable {
            eprintln!("skipped: a process that is root can remove what it likes");
            return;
        }

        let scratch = scratch();
        let root = scratch.root().to_path_buf();
        let locked = scratch.tmp().join("locked");
        fs::create_dir(&locked).expect("a directory");
        fs::write(locked.join("file"), b"x").expect("a file");
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).expect("chmod");

        let closed = scratch.close();

        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("chmod back");
        let error = closed.expect_err("what is in a directory that cannot be written stays");
        assert!(
            error.to_string().contains(&*root.to_string_lossy()),
            "the error names the area: {error}"
        );
        assert!(root.exists(), "the area that could not be removed is left");
        fs::remove_dir_all(root).expect("clean up the area that was left");
    }

    /// Set in the child process of the umask test, to the umask it runs under, written in octal.
    #[cfg(unix)]
    const UMASK_CHILD: &str = "XH_SCRATCH_TEST_UMASK_CHILD";

    /// Set in the child process of the umask test, to the directory that it makes its area in.
    #[cfg(unix)]
    const UMASK_CHILD_DIR: &str = "XH_SCRATCH_TEST_UMASK_CHILD_DIR";

    /// The umask of a process cannot be changed by one test without changing it for every other
    /// that runs at the same time in the same process, so this test runs itself again, and only
    /// itself, in a child process that a shell starts under each umask: the permissive ones that a
    /// directory is left open by (`000`, and the usual `022`), a restrictive one, and the two that
    /// take a bit from the owner of what is made (`577` leaves a directory that cannot be entered,
    /// and `277` one that cannot be written).
    #[cfg(unix)]
    #[test]
    fn an_area_is_private_to_its_owner_whatever_the_umask_of_the_process() {
        use std::{os::unix::fs::PermissionsExt as _, process::Command};

        const NAME: &str = "an_area_is_private_to_its_owner_whatever_the_umask_of_the_process";
        let mode_of =
            |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;

        if let (Some(umask), Some(parent)) = (
            std::env::var_os(UMASK_CHILD),
            std::env::var_os(UMASK_CHILD_DIR),
        ) {
            // The child: the shell that started it set the umask.
            let umask =
                u32::from_str_radix(umask.to_str().expect("text"), 8).expect("an octal umask");
            let parent = PathBuf::from(parent);
            let probe = parent.join("probe");
            fs::write(&probe, b"x").expect("a probe file");
            assert_eq!(
                mode_of(&probe),
                0o666 & !umask,
                "the umask is in force, and a file made with the default mode has what it leaves"
            );

            let area = Scratch::create(&parent).expect("an area under any umask");

            assert_eq!(mode_of(area.root()), 0o700, "the root of the area");
            for (name, directory) in [
                ("home", area.home()),
                ("config", area.config_dir()),
                ("cwd", area.cwd()),
                ("store", area.store()),
                ("tmp", area.tmp()),
            ] {
                assert_eq!(mode_of(&directory), 0o700, "{name}");
            }
            assert_eq!(
                mode_of(&area.config_file()),
                0o600,
                "the configuration file"
            );
            assert_eq!(
                fs::read_to_string(area.config_file()).expect("the configuration is readable"),
                EMPTY_CONFIG
            );
            println!("xh-umask-child: the area is private under umask {umask:04o}");
            return;
        }

        // The parent: one child for each umask, with a parent directory that is made here, under
        // the umask of this process, so that the child can enter it.
        let this_test = format!(
            "{}::{NAME}",
            module_path!().split_once("::").map_or("", |(_, path)| path)
        );
        for umask in ["000", "022", "077", "577", "277"] {
            let parent = tempfile::tempdir().expect("a directory");

            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(format!("umask {umask} && exec \"$0\" \"$@\""))
                .arg(std::env::current_exe().expect("the test binary"))
                .args(["--exact", &this_test, "--nocapture", "--test-threads=1"])
                .env(UMASK_CHILD, umask)
                .env(UMASK_CHILD_DIR, parent.path())
                .output()
                .expect("the test binary runs");

            let said = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(
                output.status.success() && said.contains("xh-umask-child: "),
                "under umask {umask}: {said}"
            );
        }
    }
}
