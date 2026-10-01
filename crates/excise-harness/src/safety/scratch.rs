//! The per-run scratch area and its exact residue check.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use tempfile::TempDir;
use thiserror::Error;

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
/// anything it reports was left behind by the run. Dropping a `Scratch` deletes the tree unless
/// [`Scratch::keep`] was called.
#[derive(Debug)]
pub struct Scratch {
    dir: Option<TempDir>,
    root: PathBuf,
    kept: bool,
}

impl Scratch {
    /// Creates a scratch area in a new private directory below `parent`.
    ///
    /// # Errors
    ///
    /// Returns an error if the directories or the configuration file cannot be created.
    pub fn create(parent: &Path) -> Result<Self, ScratchError> {
        let dir = tempfile::Builder::new()
            .prefix("xh-scratch-")
            .tempdir_in(parent)
            .map_err(|source| ScratchError::new(parent, source))?;
        let root = dir.path().to_path_buf();
        let scratch = Self {
            dir: Some(dir),
            root,
            kept: false,
        };
        for directory in [
            scratch.home(),
            scratch.config_dir(),
            scratch.cwd(),
            scratch.store(),
            scratch.tmp(),
        ] {
            fs::create_dir(&directory).map_err(|source| ScratchError::new(&directory, source))?;
        }
        let config = scratch.config_file();
        fs::write(&config, EMPTY_CONFIG).map_err(|source| ScratchError::new(&config, source))?;
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

#[cfg(test)]
mod tests {
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
}
