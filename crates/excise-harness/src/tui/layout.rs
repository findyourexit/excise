//! The files of a session: where they are, what is in them, and how they are written.
//!
//! A session has two directories.
//!
//! * The **session directory**, `<target dir>/excise-tui/<id>/`, holds the channel the commands
//!   talk to the supervisor through, and nothing else: `config.json` (what `open` asked for),
//!   `state.json` (what the supervisor has set up), `supervisor.lock` (held for as long as the
//!   supervisor lives), `supervisor.log` (its standard error), `ready.json` or `failed.json` (the
//!   answer to `open`), and the request and reply directories `req/` and `rep/`. It is private to
//!   the user (mode `0700`) and is removed when the session ends.
//! * The **workspace**, `<work base>/xh-tui-<id>/`, holds what the program works on: the run copy
//!   of the fixture and the scratch area (`HOME`, configuration, working directory, scan store,
//!   temporary directory, and event file) the program is given. It lives beside where the
//!   scenario runner puts its fixtures (`runner::work_base`), because the deletion dialog is at
//!   most 78 columns wide and a path cut short proves nothing: a workspace below a long
//!   `target/` path would make every deletion refuse. It is removed when the session ends too.

use std::{
    collections::hash_map::RandomState,
    ffi::OsStr,
    fmt, fs,
    fs::DirBuilder,
    hash::{BuildHasher, Hasher},
    io,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::{Path, PathBuf},
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{
    fixture::tree::remove_tree, report::tui::Size, safety::ProfileSettings, scenario::Profile,
};

use super::identity::ProcessIdentity;

/// The name of the directory sessions live in, below the target directory.
pub(super) const STATE_DIR_NAME: &str = "excise-tui";
/// The start of the name of the file in a workspace that says which session owns it. The file is
/// empty and its name holds the session's id and nonce ([`marker_name`]), so that it is there
/// whole or not at all, however its maker ends: nothing reads a half-written marker.
const WORKSPACE_MARKER_PREFIX: &str = ".excise-tui-workspace-";
/// The lock the supervisor holds.
const LOCK: &str = "supervisor.lock";
const CONFIG: &str = "config.json";
const STATE: &str = "state.json";
const LOG: &str = "supervisor.log";
const READY: &str = "ready.json";
const FAILED: &str = "failed.json";
const REQUESTS: &str = "req";
const REPLIES: &str = "rep";
/// How many ids to try before giving up on finding an unused one.
const ID_ATTEMPTS: usize = 32;

/// A session id: eight lowercase hexadecimal digits. It is a directory name and part of a
/// workspace name, so it can never hold a separator or a `..`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionId(String);

impl SessionId {
    const LEN: usize = 8;

    /// Checks that `text` is a session id.
    pub(super) fn parse(text: &str) -> Result<Self, String> {
        if text.len() == Self::LEN
            && text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            Ok(Self(text.to_owned()))
        } else {
            Err(format!(
                "`{text}` is not a session id: a session id is {} lowercase hexadecimal digits, \
                 as `open` prints it and `list` shows it",
                Self::LEN
            ))
        }
    }

    /// An id that nothing guarantees is unused: the caller creates the directory and tries again
    /// on a clash.
    fn random() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);

        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u32(process::id());
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_u128(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos()),
        );
        let bits = u32::try_from(hasher.finish() >> 32).unwrap_or_default();
        Self(format!("{bits:08x}"))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SessionId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What `open` asked for, written before the supervisor starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Config {
    /// The session id.
    pub(super) session: String,
    /// The fixture's id.
    pub(super) fixture: String,
    /// The profile.
    pub(super) profile: Profile,
    /// The terminal width the user asked for; a profile can fix it.
    pub(super) cols: u16,
    /// The terminal height.
    pub(super) rows: u16,
    /// Where the asciicast goes, when the session records.
    pub(super) record: Option<PathBuf>,
    /// How long the session may go without a command before it ends.
    pub(super) idle_timeout_ms: u64,
    /// The `excise` binary.
    pub(super) binary: PathBuf,
    /// The directory the workspace is made in.
    pub(super) work_base: PathBuf,
    /// When the session was opened, as an RFC 3339 timestamp.
    pub(super) started_at: String,
    /// The session's mark: a random value that its program and its workspace carry, and nothing
    /// else does (see [`super::identity`]). `open` makes it before the supervisor exists, so
    /// every process the session starts has it, and nothing is signaled or removed for the
    /// session without it.
    pub(super) nonce: String,
}

impl Config {
    /// The size of the terminal the program runs in: the width the user asked for, unless the
    /// profile fixes it, and the height they asked for.
    pub(super) fn terminal_size(&self) -> Size {
        Size {
            cols: ProfileSettings::for_profile(self.profile)
                .cols
                .unwrap_or(self.cols),
            rows: self.rows,
        }
    }
}

/// What the supervisor has set up, so that a session whose supervisor died can still be cleaned.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    /// The supervisor's process id.
    pub(super) supervisor_pid: u32,
    /// Whether the session is ready for commands.
    pub(super) ready: bool,
    /// The run copy of the fixture, once there is one.
    pub(super) root: Option<PathBuf>,
    /// The program's process id, which is its process group id, once it runs.
    pub(super) child_pid: Option<u32>,
    /// What the system said about the program right after it was started: a process id alone does
    /// not say which process is meant, once the process is gone and the number goes to another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) child: Option<ProcessIdentity>,
}

/// The directory of one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SessionDir {
    id: SessionId,
    path: PathBuf,
}

impl SessionDir {
    /// The directory of session `id` below `state_dir`.
    pub(super) fn at(state_dir: &Path, id: SessionId) -> Self {
        let path = state_dir.join(id.as_str());
        Self { id, path }
    }

    /// Creates a new, private session directory with an unused id.
    pub(super) fn create(state_dir: &Path) -> io::Result<Self> {
        Self::create_with(state_dir, SessionId::random)
    }

    /// [`SessionDir::create`], with the ids drawn by `draw`.
    fn create_with(state_dir: &Path, mut draw: impl FnMut() -> SessionId) -> io::Result<Self> {
        for _ in 0..ID_ATTEMPTS {
            let session = Self::at(state_dir, draw());
            // A recording outlives its session's directory, so a name whose recording is still
            // there is not unused: the new session would write over it.
            if fs::symlink_metadata(recording_path(state_dir, session.id())).is_ok() {
                continue;
            }
            match private_dir().create(&session.path) {
                Ok(()) => {
                    private_dir().create(session.requests())?;
                    private_dir().create(session.replies())?;
                    return Ok(session);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot find an unused session id",
        ))
    }

    pub(super) const fn id(&self) -> &SessionId {
        &self.id
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn lock(&self) -> PathBuf {
        self.path.join(LOCK)
    }

    pub(super) fn log(&self) -> PathBuf {
        self.path.join(LOG)
    }

    pub(super) fn ready(&self) -> PathBuf {
        self.path.join(READY)
    }

    pub(super) fn failed(&self) -> PathBuf {
        self.path.join(FAILED)
    }

    pub(super) fn requests(&self) -> PathBuf {
        self.path.join(REQUESTS)
    }

    pub(super) fn replies(&self) -> PathBuf {
        self.path.join(REPLIES)
    }

    /// Whether the directory exists as a real directory, not a link to one.
    pub(super) fn exists(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| metadata.is_dir())
    }

    pub(super) fn write_config(&self, config: &Config) -> io::Result<()> {
        write_json(&self.path.join(CONFIG), config)
    }

    pub(super) fn read_config(&self) -> io::Result<Config> {
        read_json(&self.path.join(CONFIG))
    }

    pub(super) fn write_state(&self, state: &State) -> io::Result<()> {
        write_json(&self.path.join(STATE), state)
    }

    pub(super) fn read_state(&self) -> Option<State> {
        read_json(&self.path.join(STATE)).ok()
    }

    /// Removes the session directory and everything in it.
    ///
    /// The lock goes after the rest, so that a directory that is being removed still looks like a
    /// live session to anyone who asks, and the configuration goes last of all: it is what proves
    /// that the directory is the driver's, so a removal that is interrupted leaves what the next
    /// sweep needs to finish it.
    pub(super) fn remove(&self) -> io::Result<()> {
        self.remove_with(&mut remove_path)
    }

    /// [`SessionDir::remove`], with the removal of each path done by `remove`.
    fn remove_with(&self, remove: &mut dyn FnMut(&Path) -> io::Result<()>) -> io::Result<()> {
        let mut paths = match fs::read_dir(&self.path) {
            Ok(entries) => entries
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<io::Result<Vec<_>>>()?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        // However the file system lists them: everything else, then the lock, then the
        // configuration. The sort is stable, so the rest stays as it was.
        paths.sort_by_key(|path| match path.file_name() {
            Some(name) if name == CONFIG => 2,
            Some(name) if name == LOCK => 1,
            _ => 0,
        });
        for path in &paths {
            remove(path)?;
        }
        fs::remove_dir(&self.path)
    }
}

/// Removes the file or directory at `path`; one that is not there is already removed.
fn remove_path(path: &Path) -> io::Result<()> {
    let removed = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(path),
        Ok(_) => fs::remove_file(path),
        Err(error) => Err(error),
    };
    match removed {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// A builder for a directory only its owner can use.
fn private_dir() -> DirBuilder {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    builder
}

/// What the path that should be the directory sessions live in is: `Ok(false)` when nothing is
/// there (there are no sessions then), `Ok(true)` when it is a real directory, and an error when
/// it is a link or anything else.
///
/// The sessions of a directory that is reached through a link are not the driver's to look at,
/// still less to clean: whatever the link leads to is not what the driver made, and a cleaner
/// that follows it removes things that are not its own. Every command that looks at the sessions
/// asks this first.
pub(super) fn check_state_dir(state_dir: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(state_dir) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(io::Error::other(format!(
            "`{}` is a link or is not a directory, so no session in it is looked at or cleaned; \
             remove it and the driver makes a directory of its own",
            state_dir.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

/// Creates the directory sessions live in, if it is not there yet, and makes sure that only its
/// owner can use it: a recording is made in it, and a request a session serves is read from it.
///
/// # Errors
///
/// Returns an error if the path is a link or not a directory, or if the directory is open to
/// others and cannot be made private (it is not the user's).
pub(super) fn ensure_state_dir(state_dir: &Path) -> io::Result<()> {
    private_dir().recursive(true).create(state_dir)?;
    check_state_dir(state_dir)?;
    let metadata = fs::symlink_metadata(state_dir)?;
    if metadata.permissions().mode() & 0o077 != 0 {
        fs::set_permissions(state_dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The sessions in `state_dir`: its directories whose names are session ids. Links, files, and
/// anything else are not sessions, and `state_dir` itself must not be a link (see
/// [`check_state_dir`]), so nothing the driver did not make is ever reached.
pub(super) fn sessions(state_dir: &Path) -> io::Result<Vec<SessionDir>> {
    if !check_state_dir(state_dir)? {
        return Ok(Vec::new());
    }
    let entries = match fs::read_dir(state_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry?;
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|name| SessionId::parse(name).ok())
        else {
            continue;
        };
        if entry.file_type()?.is_dir() {
            found.push(SessionDir::at(state_dir, id));
        }
    }
    found.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    Ok(found)
}

/// Where an asciicast of session `id` is kept: beside the session directory, so it outlives it.
pub(super) fn recording_path(state_dir: &Path, id: &SessionId) -> PathBuf {
    state_dir.join(format!("{id}.cast"))
}

/// The workspace of session `id` in `work_base`.
pub(super) fn workspace_path(work_base: &Path, id: &SessionId) -> PathBuf {
    work_base.join(format!("xh-tui-{id}"))
}

/// How old an empty workspace without a marker has to be before it counts as one that a killed
/// supervisor left, not one that another supervisor is making this very moment.
const EMPTY_WORKSPACE_GRACE: Duration = Duration::from_mins(1);

/// What a workspace's marker is called: the session's id and nonce are in its name.
fn marker_name(id: &SessionId, nonce: &str) -> String {
    format!("{WORKSPACE_MARKER_PREFIX}{id}-{nonce}")
}

/// A workspace that this process made, and holds the proof of: [`Workspace::create`] makes the
/// directory itself and fails when anything is already at the path, so a value of this type exists
/// only for a directory that this session's supervisor created.
///
/// That matters because workspaces of every checkout share one directory (the work base), while a
/// session id is only 32 random bits, unique within one session directory: two checkouts can pick
/// the same id. The second cannot create its workspace, and it must not remove the first's, which
/// carries the same id. Only a holder of this type removes a workspace without being asked to by a
/// session's configuration, and its marker holds the session's nonce besides its id.
#[derive(Debug, Clone)]
pub(super) struct Workspace {
    work_base: PathBuf,
    id: SessionId,
    nonce: String,
}

impl Workspace {
    /// Creates the workspace of session `id`: a private directory whose marker names the session
    /// and its nonce.
    ///
    /// # Errors
    ///
    /// Returns an error, with the kind `AlreadyExists` if anything is at the path already, in
    /// which case nothing was made and nothing there is touched.
    pub(super) fn create(work_base: &Path, id: &SessionId, nonce: &str) -> io::Result<Self> {
        let path = workspace_path(work_base, id);
        private_dir().create(&path)?;
        let marker = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path.join(marker_name(id, nonce)));
        if let Err(error) = marker {
            // This directory was made a moment ago and holds nothing, and nothing removes a
            // workspace that does not say whose it is.
            let _ = fs::remove_dir(&path);
            return Err(error);
        }
        Ok(Self {
            work_base: work_base.to_path_buf(),
            id: id.clone(),
            nonce: nonce.to_owned(),
        })
    }

    /// The directory.
    pub(super) fn path(&self) -> PathBuf {
        workspace_path(&self.work_base, &self.id)
    }

    /// Removes the workspace and what is in it, once more checking that it is this one.
    pub(super) fn remove(&self) -> Result<(), String> {
        remove_workspace(&self.work_base, &self.id, &self.nonce)
    }
}

/// Removes the workspace of session `id`, if there is one, and only if it is the session's: a real
/// directory, named for the session, that holds the marker of the session and its nonce `nonce`.
/// Anything else at that path is left alone and reported, among it a workspace of another
/// checkout's session that has the same id and another nonce.
///
/// The marker is the last thing removed, so that a removal that is interrupted leaves a workspace
/// that still says whose it is, and the next removal finishes it.
pub(super) fn remove_workspace(
    work_base: &Path,
    id: &SessionId,
    nonce: &str,
) -> Result<(), String> {
    let path = workspace_path(work_base, id);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() => metadata,
        Ok(_) => {
            return Err(format!(
                "`{}` is not a directory, so it is not this session's workspace and was left \
                 alone",
                path.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("cannot look at `{}`: {error}", path.display())),
    };
    let marker = marker_name(id, nonce);
    let (own, another) = markers(&path, &marker)
        .map_err(|error| format!("cannot look into `{}`: {error}", path.display()))?;
    if another {
        return Err(format!(
            "`{}` carries another session's marker, so it was left alone",
            path.display()
        ));
    }
    if !own {
        // A supervisor killed between making the directory and making its marker leaves an
        // empty one, which holds nothing to lose, and no marker will ever be made in it. One
        // that is young may be a supervisor's that is about to make it.
        let old = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= EMPTY_WORKSPACE_GRACE);
        if old && fs::remove_dir(&path).is_ok() {
            return Ok(());
        }
        return Err(format!(
            "`{}` does not carry this session's marker, so it was left alone",
            path.display()
        ));
    }
    remove_marked_workspace(&path, &marker, &mut remove_tree)
        .map_err(|error| format!("cannot remove `{}`: {error}", path.display()))
}

/// Whether the workspace at `path` holds the marker called `own`, as a file of its own, and
/// whether it holds the marker of any other session.
fn markers(path: &Path, own: &str) -> io::Result<(bool, bool)> {
    let mut found = (false, false);
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == own {
            found.0 = entry.file_type()?.is_file();
        } else if name.starts_with(WORKSPACE_MARKER_PREFIX) {
            found.1 = true;
        }
    }
    Ok(found)
}

/// Removes everything in the workspace at `path` by `remove`, then its marker `marker`, then the
/// workspace itself.
fn remove_marked_workspace(
    path: &Path,
    marker: &str,
    remove: &mut dyn FnMut(&Path) -> io::Result<()>,
) -> io::Result<()> {
    let mut paths = fs::read_dir(path)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    // However the file system lists them, the marker is last. The sort is stable.
    paths.sort_by_key(|entry| {
        entry
            .file_name()
            .is_some_and(|name| name == OsStr::new(marker))
    });
    for entry in &paths {
        remove(entry)?;
    }
    fs::remove_dir(path)
}

/// Writes `bytes` to `path` so that a reader sees all of it or none: into a file of its own name
/// first, then renamed over `path`.
pub(super) fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no file name"))?;
    let mut temporary = name.to_os_string();
    temporary.push(format!(
        ".tmp-{}-{}",
        process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = path.with_file_name(temporary);
    let result = fs::write(&temporary, bytes).and_then(|()| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    write_atomic(path, &bytes)
}

/// Whether `name` is the name of a temporary file that [`write_atomic`] makes for the session's
/// configuration: `config.json.tmp-<process id>-<counter>`. An `open` that is killed between
/// writing it and renaming it leaves one behind.
pub(super) fn is_config_temporary(name: &str) -> bool {
    name.strip_prefix(CONFIG)
        .and_then(|rest| rest.strip_prefix(".tmp-"))
        .and_then(|rest| rest.split_once('-'))
        .is_some_and(|(process, counter)| {
            [process, counter].iter().all(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
        })
}

pub(super) fn read_json<T: DeserializeOwned>(path: &Path) -> io::Result<T> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("`{}` is not valid: {error}", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NONCE: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn a_session_id_is_eight_lowercase_hexadecimal_digits_and_nothing_else() {
        assert!(SessionId::parse("0123abcd").is_ok());
        for text in [
            "",
            "0123abc",
            "0123abcde",
            "0123ABCD",
            "0123abcg",
            "../../ab",
            "0123abc/",
            "0123 abc",
            "０１２３abcd",
        ] {
            assert!(SessionId::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn a_random_id_is_a_valid_id() {
        for _ in 0..100 {
            let id = SessionId::random();

            assert!(SessionId::parse(id.as_str()).is_ok(), "{id}");
        }
    }

    #[test]
    fn creating_session_directories_never_reuses_a_name_and_makes_them_private() {
        use std::os::unix::fs::PermissionsExt as _;

        let state = tempfile::tempdir().expect("a state directory");
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..50 {
            let session = SessionDir::create(state.path()).expect("a session directory");

            assert!(seen.insert(session.id().to_string()));
            let mode =
                |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode(session.path()), 0o700);
            assert_eq!(mode(&session.requests()), 0o700);
            assert_eq!(mode(&session.replies()), 0o700);
        }
        assert_eq!(sessions(state.path()).expect("sessions").len(), 50);
    }

    #[test]
    fn only_real_directories_named_like_ids_are_sessions() {
        let state = tempfile::tempdir().expect("a state directory");
        let real = SessionDir::create(state.path()).expect("a session directory");
        fs::create_dir(state.path().join("not-an-id")).expect("a directory");
        fs::write(state.path().join("0123abcd"), b"a file").expect("a file");
        fs::write(state.path().join("0123abcd.cast"), b"a recording").expect("a recording");
        std::os::unix::fs::symlink(real.path(), state.path().join("deadbeef")).expect("a link");

        let found = sessions(state.path()).expect("sessions");

        assert_eq!(
            found
                .iter()
                .map(|session| session.id().as_str())
                .collect::<Vec<_>>(),
            [real.id().as_str()]
        );
        assert!(
            sessions(&state.path().join("absent"))
                .expect("absent")
                .is_empty()
        );
    }

    #[test]
    fn removing_a_session_directory_leaves_nothing() {
        let state = tempfile::tempdir().expect("a state directory");
        let session = SessionDir::create(state.path()).expect("a session directory");
        fs::write(session.lock(), b"").expect("the lock file");
        fs::write(session.log(), b"log").expect("the log");
        fs::write(session.requests().join("a.json"), b"{}").expect("a request");

        session.remove().expect("removal");

        assert!(!session.exists());
        assert!(
            fs::read_dir(state.path())
                .expect("the state directory")
                .next()
                .is_none()
        );
        session
            .remove()
            .expect("removing what is gone is not an error");
    }

    #[test]
    fn removing_a_session_directory_takes_the_lock_and_then_the_configuration_last() {
        let state = tempfile::tempdir().expect("a state directory");
        let session = SessionDir::create(state.path()).expect("a session directory");
        fs::write(session.lock(), b"").expect("the lock file");
        fs::write(session.path().join(CONFIG), b"{}").expect("the configuration");
        fs::write(session.path().join(STATE), b"{}").expect("the state");
        fs::write(session.log(), b"log").expect("the log");
        let mut removed = Vec::new();

        session
            .remove_with(&mut |path| {
                removed.push(
                    path.file_name()
                        .expect("a name")
                        .to_string_lossy()
                        .into_owned(),
                );
                remove_path(path)
            })
            .expect("removal");

        let last: Vec<&str> = removed.iter().rev().take(2).map(String::as_str).collect();
        assert_eq!(last, [CONFIG, LOCK], "removed in this order: {removed:?}");
        assert!(!session.exists());
    }

    #[test]
    fn a_removal_that_is_interrupted_leaves_the_configuration_for_the_next_one() {
        let state = tempfile::tempdir().expect("a state directory");
        let session = SessionDir::create(state.path()).expect("a session directory");
        fs::write(session.lock(), b"").expect("the lock file");
        fs::write(session.path().join(CONFIG), b"{}").expect("the configuration");
        fs::write(session.log(), b"log").expect("the log");

        let interrupted = session.remove_with(&mut |path| {
            if path.file_name().is_some_and(|name| name == LOCK) {
                return Err(io::Error::other("interrupted"));
            }
            remove_path(path)
        });

        assert!(interrupted.is_err());
        assert!(
            session.path().join(CONFIG).is_file(),
            "what proves that the directory is the driver's is still there"
        );
        assert!(!session.log().exists());
        session.remove().expect("the next removal finishes it");
        assert!(!session.exists());
    }

    #[test]
    fn a_session_id_is_not_drawn_again_while_its_recording_is_still_there() {
        let state = tempfile::tempdir().expect("a state directory");
        let taken = SessionId::parse("aaaaaaaa").expect("an id");
        let free = SessionId::parse("bbbbbbbb").expect("an id");
        let recording = recording_path(state.path(), &taken);
        fs::write(&recording, b"a recording that was kept").expect("a recording");
        let mut draws = [taken.clone(), free.clone()].into_iter();

        let session = SessionDir::create_with(state.path(), || draws.next().expect("an id"))
            .expect("a session directory");

        assert_eq!(session.id(), &free);
        assert!(!SessionDir::at(state.path(), taken).exists());
        assert_eq!(
            fs::read(&recording).expect("the recording"),
            b"a recording that was kept"
        );
    }

    #[test]
    fn config_and_state_round_trip_and_reject_unknown_fields() {
        let state = tempfile::tempdir().expect("a state directory");
        let session = SessionDir::create(state.path()).expect("a session directory");
        let config = Config {
            session: session.id().to_string(),
            fixture: "delete-file".to_owned(),
            profile: Profile::Default,
            cols: 120,
            rows: 40,
            record: Some(recording_path(state.path(), session.id())),
            idle_timeout_ms: 900_000,
            binary: PathBuf::from("/bin/excise"),
            work_base: PathBuf::from("/tmp"),
            started_at: "2026-10-04T00:00:00.000Z".to_owned(),
            nonce: NONCE.to_owned(),
        };
        session.write_config(&config).expect("config");
        let supervisor = State {
            supervisor_pid: 7,
            ready: true,
            root: Some(PathBuf::from("/tmp/xh-tui-x/root")),
            child_pid: Some(8),
            child: Some(ProcessIdentity {
                pid: 8,
                started: 1_790_000_000,
                exe: Some("/usr/local/bin/excise".to_owned()),
            }),
        };
        session.write_state(&supervisor).expect("state");

        assert_eq!(session.read_config().expect("config"), config);
        assert_eq!(session.read_state(), Some(supervisor));
        fs::write(
            session.path().join("state.json"),
            r#"{"supervisor_pid":1,"ready":false,"root":null,"child_pid":null,"extra":1}"#,
        )
        .expect("a state with a field this build does not know");
        assert_eq!(session.read_state(), None);
    }

    #[test]
    fn a_workspace_is_removed_only_when_it_is_this_sessions() {
        let base = tempfile::tempdir().expect("a work base");
        let mine = SessionId::parse("0123abcd").expect("an id");
        let other = SessionId::parse("deadbeef").expect("an id");

        let workspace = Workspace::create(base.path(), &mine, NONCE).expect("a workspace");
        let path = workspace.path();
        fs::create_dir_all(path.join("fixture/deep")).expect("contents");
        fs::write(path.join("fixture/deep/file"), b"x").expect("a file");
        assert_eq!(path, workspace_path(base.path(), &mine));
        assert_eq!(
            fs::metadata(&path).expect("metadata").permissions().mode() & 0o777,
            0o700
        );

        // Another session's id names another workspace, which does not exist.
        assert_eq!(remove_workspace(base.path(), &other, NONCE), Ok(()));
        assert!(path.exists());

        // A directory that merely has the right name, without the marker, is left alone...
        let impostor = workspace_path(base.path(), &other);
        fs::create_dir(&impostor).expect("an impostor");
        fs::write(impostor.join("precious"), b"keep").expect("a file");
        let refused = remove_workspace(base.path(), &other, NONCE).expect_err("an impostor");
        assert!(refused.contains("does not carry"), "{refused}");
        assert!(impostor.join("precious").exists());

        // ...and so is a link with the right name.
        let linked = SessionId::parse("cafe0000").expect("an id");
        std::os::unix::fs::symlink(&impostor, workspace_path(base.path(), &linked))
            .expect("a link");
        let refused = remove_workspace(base.path(), &linked, NONCE).expect_err("a link");
        assert!(refused.contains("not a directory"), "{refused}");
        assert!(impostor.join("precious").exists());

        // The workspace of another checkout's session that drew the same id carries another
        // session's nonce.
        let refused =
            remove_workspace(base.path(), &mine, "another-nonce").expect_err("another session");
        assert!(refused.contains("another session's marker"), "{refused}");
        assert!(path.join("fixture/deep/file").exists());

        assert_eq!(workspace.remove(), Ok(()));
        assert!(!path.exists());
        assert_eq!(workspace.remove(), Ok(()), "what is gone is gone");
    }

    #[test]
    fn a_workspace_is_never_made_over_something_that_is_there() {
        // A session id is 32 random bits drawn in the state directory of one checkout, and every
        // checkout makes its workspaces in the same directory, so two sessions can draw the same
        // id. The second must fail, and leave the first's workspace as it is.
        let base = tempfile::tempdir().expect("a work base");
        let id = SessionId::parse("0123abcd").expect("an id");
        let first = Workspace::create(base.path(), &id, "first-nonce").expect("a workspace");
        fs::write(first.path().join("precious"), b"keep").expect("a file");

        let clash = Workspace::create(base.path(), &id, "second-nonce").expect_err("a clash");

        assert_eq!(clash.kind(), io::ErrorKind::AlreadyExists);
        assert!(first.path().join("precious").exists());
        let marker = first.path().join(marker_name(&id, "first-nonce"));
        assert!(marker.is_file());
        assert_eq!(
            fs::metadata(&marker).expect("the marker").len(),
            0,
            "the marker has no contents that a kill could leave half written"
        );
        let linked = SessionId::parse("deadbeef").expect("an id");
        std::os::unix::fs::symlink(base.path(), workspace_path(base.path(), &linked))
            .expect("a link");
        let through_a_link =
            Workspace::create(base.path(), &linked, NONCE).expect_err("a link is something");
        assert_eq!(through_a_link.kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn a_workspace_loses_its_marker_after_everything_else() {
        let base = tempfile::tempdir().expect("a work base");
        let id = SessionId::parse("0123abcd").expect("an id");
        let workspace = Workspace::create(base.path(), &id, NONCE).expect("a workspace");
        let path = workspace.path();
        fs::create_dir_all(path.join("run-copy/deep")).expect("contents");
        fs::write(path.join("scratch"), b"x").expect("a file");
        let marker = marker_name(&id, NONCE);

        // A removal that stops partway leaves the marker, so that the next one knows whose it is.
        let mut calls = 0;
        let interrupted = remove_marked_workspace(&path, &marker, &mut |entry| {
            calls += 1;
            if calls == 2 {
                Err(io::Error::other("interrupted"))
            } else {
                remove_tree(entry)
            }
        });
        assert!(interrupted.is_err());
        assert!(path.join(&marker).is_file());
        assert_eq!(remove_workspace(base.path(), &id, NONCE), Ok(()));
        assert!(!path.exists());

        // Whatever the order of the entries, the marker is the last to go.
        let workspace = Workspace::create(base.path(), &id, NONCE).expect("a workspace");
        fs::create_dir_all(workspace.path().join("run-copy")).expect("contents");
        let mut removed = Vec::new();
        remove_marked_workspace(&workspace.path(), &marker, &mut |entry| {
            removed.push(
                entry
                    .file_name()
                    .expect("a name")
                    .to_string_lossy()
                    .into_owned(),
            );
            remove_tree(entry)
        })
        .expect("removal");
        assert_eq!(
            removed.last(),
            Some(&marker),
            "removed in this order: {removed:?}"
        );
        assert!(!workspace.path().exists());
    }

    #[test]
    fn an_empty_workspace_without_a_marker_goes_once_it_is_old_enough_to_be_a_killed_ones() {
        let base = tempfile::tempdir().expect("a work base");
        let id = SessionId::parse("0123abcd").expect("an id");
        let path = workspace_path(base.path(), &id);
        let old = SystemTime::now() - Duration::from_hours(1);
        let age = |path: &Path| {
            fs::File::open(path)
                .and_then(|file| file.set_modified(old))
                .expect("an old directory");
        };
        fs::create_dir(&path).expect("a directory that was made, and then its maker was killed");

        // A young one may be a supervisor's that is about to write its marker.
        let refused = remove_workspace(base.path(), &id, NONCE).expect_err("a young directory");
        assert!(refused.contains("does not carry"), "{refused}");
        assert!(path.exists());

        age(&path);
        assert_eq!(remove_workspace(base.path(), &id, NONCE), Ok(()));
        assert!(!path.exists());

        // An old directory with anything in it is not empty, and nothing of it is removed.
        fs::create_dir(&path).expect("a directory");
        fs::write(path.join("precious"), b"keep").expect("a file");
        age(&path);
        assert!(remove_workspace(base.path(), &id, NONCE).is_err());
        assert!(path.join("precious").exists());
    }

    #[test]
    fn the_state_directory_is_private_and_is_not_a_link() {
        let base = tempfile::tempdir().expect("a base");
        let mode = |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;

        let loose = base.path().join("loose");
        fs::create_dir(&loose).expect("a directory");
        fs::set_permissions(&loose, fs::Permissions::from_mode(0o755)).expect("loose");
        ensure_state_dir(&loose).expect("a directory of the user's is made private");
        assert_eq!(mode(&loose), 0o700);

        let fresh = base.path().join("deep/er/state");
        ensure_state_dir(&fresh).expect("a new directory");
        assert_eq!(mode(&fresh), 0o700);

        let link = base.path().join("link");
        std::os::unix::fs::symlink(&loose, &link).expect("a link");
        let error = ensure_state_dir(&link).expect_err("a link");
        assert!(error.to_string().contains("not a directory"), "{error}");

        let file = base.path().join("file");
        fs::write(&file, b"x").expect("a file");
        assert!(ensure_state_dir(&file).is_err());
    }

    #[test]
    fn the_temporary_file_of_the_configuration_is_known_by_its_name_and_nothing_else_is() {
        assert!(is_config_temporary("config.json.tmp-1-0"));
        assert!(is_config_temporary(&format!(
            "{CONFIG}.tmp-{}-12",
            process::id()
        )));
        for name in [
            "config.json",
            "config.json.tmp-1",
            "config.json.tmp--1",
            "config.json.tmp-1-",
            "config.json.tmp-x-1",
            "config.json.tmp-1-0.bak",
            "state.json.tmp-1-0",
        ] {
            assert!(!is_config_temporary(name), "{name}");
        }
    }

    #[test]
    fn an_atomic_write_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("doc.json");

        write_atomic(&path, b"first").expect("a write");
        write_atomic(&path, b"second").expect("an overwrite");

        assert_eq!(fs::read(&path).expect("the file"), b"second");
        assert_eq!(fs::read_dir(dir.path()).expect("the directory").count(), 1);
    }
}
