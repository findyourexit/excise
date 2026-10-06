//! Constrained scratch volumes: a small, size-limited filesystem attached at a chosen directory.
//!
//! This lets a test exercise a mount boundary and free-space behavior (`ENOSPC` and friends)
//! against something that is not the host's real disk: a fresh, tiny filesystem that a test
//! creates, fills, and destroys.
//!
//! # Opt-in
//!
//! Creating and attaching a filesystem image needs OS mount privileges (root on Linux, an
//! elevated process on Windows; no special privilege on macOS, but the mechanism is still
//! privileged-adjacent and touches host mount state). A caller must hold a [`PrivilegedOptIn`],
//! obtained either from the [`PRIVILEGED_ENV`] environment variable being set to exactly `1` (see
//! [`PrivilegedOptIn::from_env`]) or from an operator's explicit decision, e.g. a `--privileged`
//! CLI flag (see [`PrivilegedOptIn::granted`]). There is no way to construct one silently.
//!
//! # Per-OS mechanism
//!
//! * **macOS**: `hdiutil create -fs APFS -type SPARSE` builds a sparse disk image, `hdiutil
//!   attach -mountpoint` attaches it. No root needed. The opt-in test at the bottom of this file
//!   runs the whole lifecycle against the real `hdiutil`, which prints deprecation warnings that
//!   point at `diskutil image` on recent macOS releases but still works.
//! * **Linux**: the image is a plain file (`std::fs::File::set_len`) formatted with `mkfs.ext4`
//!   and mounted with `mount -o loop,nodev,nosuid`. Needs root; commands run directly if already
//!   root, otherwise prefixed with `sudo -n` (never interactive). Written from the tools'
//!   documentation and never run by its author: the first real run is a CI tier that allows
//!   privileged steps.
//! * **Windows**: a `diskpart` script creates and formats a VHD and assigns it to the mount
//!   folder. Needs an elevated (Administrator) process. Written from documentation and
//!   type-checked for `x86_64-pc-windows-msvc`, but never run; success is judged by exit status
//!   and a writability probe rather than a mount-specific API.
//!
//! Every helper command runs through one bounded runner ([`run`]) that polls for completion,
//! kills and reports a timeout past a fixed deadline, and drains stdout/stderr on background
//! threads so a full pipe cannot deadlock the wait.
//!
//! # Cleanup guarantees
//!
//! [`Volume::attach`] either returns a fully attached, current-user-writable volume, or leaves
//! nothing behind: any image file or mount created partway through a failed attempt is removed
//! before the error is returned. [`Volume::detach`] unmounts (forcing if needed) and removes the
//! image, reporting failures as [`VolumeError::Cleanup`]. [`Drop`] performs the same detach
//! best-effort and never panics; call `detach` explicitly to observe failures.

use std::{
    env,
    ffi::OsString,
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use thiserror::Error;

/// The environment variable that opts in to privileged/volume operations.
///
/// Must be set to exactly `1`; any other value (including unset, empty, `"0"`, `"true"`, or a
/// value with surrounding whitespace) does not opt in.
pub const PRIVILEGED_ENV: &str = "EXCISE_HARNESS_PRIVILEGED";

/// Proof that the operator explicitly opted in to volume operations.
///
/// The only ways to build one are [`PrivilegedOptIn::from_env`] (reading [`PRIVILEGED_ENV`]) and
/// [`PrivilegedOptIn::granted`] (an operator decision the caller already holds, e.g. a
/// `--privileged` CLI flag). Its single field is private, so it cannot be constructed with a
/// struct literal outside this module: holding one is itself the proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivilegedOptIn {
    accepted: (),
}

impl PrivilegedOptIn {
    /// Reads [`PRIVILEGED_ENV`] from the process environment. `Some` only when it is set to
    /// exactly `1`.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        opt_in_from_value(env::var(PRIVILEGED_ENV).ok().as_deref())
    }

    /// For a caller that already holds an explicit operator decision (e.g. a `--privileged` CLI
    /// flag) rather than reading the environment.
    #[must_use]
    pub const fn granted() -> Self {
        Self { accepted: () }
    }
}

/// The pure parser behind [`PrivilegedOptIn::from_env`]. Separated out because tests cannot call
/// `std::env::set_var` (unsafe in edition 2024) and so cannot exercise `from_env` directly.
pub(crate) fn opt_in_from_value(value: Option<&str>) -> Option<PrivilegedOptIn> {
    if value == Some("1") {
        Some(PrivilegedOptIn::granted())
    } else {
        None
    }
}

/// Smallest volume size this crate will create.
///
/// Measured on this machine: an APFS sparse image below this leaves too little usable space to
/// be a representative fixture (a 4 MiB image leaves only ~50% usable before `ENOSPC`, and a 1
/// MiB image leaves none at all). An 8 MiB image reliably leaves ~75% usable.
const MIN_SIZE_MIB: u32 = 8;

/// Largest volume size this crate will create. Bigger scratch volumes slow down test setup for
/// no benefit: exercising a size limit needs a small filesystem, not a realistic one.
const MAX_SIZE_MIB: u32 = 4096;

/// Longest volume label, shared by APFS, ext4, and NTFS.
const MAX_LABEL_LEN: usize = 11;

/// What to create: the size and label of a scratch volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSpec {
    /// Total size of the filesystem, in MiB. Must be between 8 and 4096 inclusive.
    pub size_mib: u32,
    /// Volume label: 1 to 11 ASCII letters, digits, `_`, or `-`, starting with a letter.
    pub label: String,
}

impl VolumeSpec {
    /// Validates `size_mib` and `label` and builds a spec.
    ///
    /// # Errors
    ///
    /// Returns [`VolumeError::InvalidSpec`] if `size_mib` is outside 8..=4096 MiB, or if `label`
    /// is empty, longer than 11 characters, does not start with an ASCII letter, or contains a
    /// character other than an ASCII letter, digit, `_`, or `-`.
    pub fn new(size_mib: u32, label: impl Into<String>) -> Result<Self, VolumeError> {
        if !(MIN_SIZE_MIB..=MAX_SIZE_MIB).contains(&size_mib) {
            return Err(VolumeError::InvalidSpec {
                reason: format!(
                    "size_mib is {size_mib}, but must be between {MIN_SIZE_MIB} and \
                     {MAX_SIZE_MIB} MiB inclusive"
                ),
            });
        }
        let label = label.into();
        validate_label(&label)?;
        Ok(Self { size_mib, label })
    }
}

fn validate_label(label: &str) -> Result<(), VolumeError> {
    if label.is_empty() {
        return Err(VolumeError::InvalidSpec {
            reason: "label must not be empty".to_string(),
        });
    }

    let char_count = label.chars().count();
    if char_count > MAX_LABEL_LEN {
        return Err(VolumeError::InvalidSpec {
            reason: format!("label must be at most {MAX_LABEL_LEN} characters, got {char_count}"),
        });
    }

    if !label.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return Err(VolumeError::InvalidSpec {
            reason: format!("label `{label}` must start with an ASCII letter"),
        });
    }

    if let Some(bad) = label
        .chars()
        .find(|&c| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
    {
        return Err(VolumeError::InvalidSpec {
            reason: format!(
                "label `{label}` contains `{bad}`, which is not an ASCII letter, digit, `_`, or `-`"
            ),
        });
    }

    Ok(())
}

/// An attached, size-limited filesystem. Detaches and deletes its image on drop.
#[derive(Debug)]
pub struct Volume {
    mount_point: PathBuf,
    image_path: PathBuf,
    spec: VolumeSpec,
    /// The whole-disk device node (e.g. `/dev/disk4`) macOS's `hdiutil attach` reported, kept
    /// only as a detach fallback if `mount_point` stops resolving. Unused on other platforms.
    macos_whole_disk: Option<String>,
    /// `false` once an explicit [`Volume::detach`] has run, so `Drop` does not repeat it.
    armed: bool,
}

impl Volume {
    /// Creates the image inside `work_dir` (an existing directory outside `mount_point`) and
    /// attaches it at `mount_point` (an existing, empty directory the caller owns). Returns only
    /// after the mount is usable by the current, unprivileged user.
    ///
    /// # Errors
    ///
    /// Returns [`VolumeError::InvalidMountPoint`] if `mount_point` does not exist, is not a
    /// directory, or is not empty; this check runs before any command or file is created.
    /// Returns [`VolumeError::Unsupported`] on a platform other than macOS, Linux, or Windows.
    /// Returns [`VolumeError::Command`] or [`VolumeError::Io`] if a helper command or filesystem
    /// operation fails. On any error nothing stays attached and no image file remains.
    pub fn attach(
        _opt_in: PrivilegedOptIn,
        spec: &VolumeSpec,
        work_dir: &Path,
        mount_point: &Path,
    ) -> Result<Self, VolumeError> {
        validate_mount_point(mount_point)?;

        match current_platform() {
            Platform::MacOs => macos::attach(spec, work_dir, mount_point),
            Platform::Linux => linux::attach(spec, work_dir, mount_point),
            Platform::Windows => windows::attach(spec, work_dir, mount_point),
            Platform::Other(os) => Err(VolumeError::Unsupported { os }),
        }
    }

    /// The directory the volume is attached at.
    #[must_use]
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// The backing image file.
    #[must_use]
    pub fn image_path(&self) -> &Path {
        &self.image_path
    }

    /// The spec this volume was created from.
    #[must_use]
    pub fn spec(&self) -> &VolumeSpec {
        &self.spec
    }

    /// Detaches the volume and removes its image, reporting any failure.
    ///
    /// `Drop` performs the same cleanup best-effort and silently if this is never called.
    ///
    /// # Errors
    ///
    /// Returns [`VolumeError::Cleanup`] if the platform unmount fails (even after a forced
    /// retry) or if the image file cannot be removed after a successful unmount.
    pub fn detach(mut self) -> Result<(), VolumeError> {
        self.armed = false;
        perform_detach(
            &self.mount_point,
            &self.image_path,
            self.macos_whole_disk.as_deref(),
        )
    }
}

impl Drop for Volume {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let _ = perform_detach(
            &self.mount_point,
            &self.image_path,
            self.macos_whole_disk.as_deref(),
        );
    }
}

fn perform_detach(
    mount_point: &Path,
    image_path: &Path,
    macos_whole_disk: Option<&str>,
) -> Result<(), VolumeError> {
    let unmount_result = match current_platform() {
        Platform::MacOs => macos::detach(mount_point, macos_whole_disk),
        Platform::Linux => linux::detach(mount_point),
        Platform::Windows => windows::detach(image_path),
        Platform::Other(os) => Err(VolumeError::Unsupported { os }),
    };

    if let Err(source_err) = unmount_result {
        return Err(VolumeError::Cleanup {
            mount_point: mount_point.to_path_buf(),
            stage: "unmount",
            reason: source_err.to_string(),
        });
    }

    fs::remove_file(image_path).map_err(|source| VolumeError::Cleanup {
        mount_point: mount_point.to_path_buf(),
        stage: "remove image",
        reason: format!("removing `{}`: {source}", image_path.display()),
    })
}

/// Typed errors for volume creation and teardown.
#[derive(Debug, Error)]
pub enum VolumeError {
    /// `size_mib` or `label` failed validation.
    #[error("invalid volume spec: {reason}")]
    InvalidSpec {
        /// Why the spec was rejected.
        reason: String,
    },

    /// `mount_point` (or, on Windows, a path embedded in a diskpart script) is unusable.
    #[error("invalid mount point `{}`: {reason}", path.display())]
    InvalidMountPoint {
        /// The rejected path.
        path: PathBuf,
        /// Why the path was rejected.
        reason: String,
    },

    /// The current platform has no volume implementation.
    #[error("scratch volumes are not supported on {os}")]
    Unsupported {
        /// `std::env::consts::OS` for the current platform.
        os: &'static str,
    },

    /// A helper command exited with a non-zero status or ran past its timeout.
    #[error("command `{program}` {} failed: {status}: {stderr}", args.join(" "))]
    Command {
        /// The program that was run.
        program: String,
        /// Its arguments, in order.
        args: Vec<String>,
        /// The exit status's `Display` form, or `"timed out after Ns"`.
        status: String,
        /// Captured stderr.
        stderr: String,
    },

    /// A filesystem operation on this process's side failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being attempted.
        context: String,
        /// The underlying error.
        #[source]
        source: io::Error,
    },

    /// Detaching or removing the image failed after the volume was attached.
    #[error("failed to clean up the volume at `{}` ({stage}): {reason}", mount_point.display())]
    Cleanup {
        /// The mount point that could not be fully cleaned up.
        mount_point: PathBuf,
        /// Which stage failed: `"unmount"` or `"remove image"`.
        stage: &'static str,
        /// The underlying failure's message.
        reason: String,
    },
}

/// Validates that `mount_point` is an existing, empty directory. Runs before any command or file
/// is created, so a bad mount point never leaves partial state behind.
fn validate_mount_point(mount_point: &Path) -> Result<(), VolumeError> {
    let metadata = fs::metadata(mount_point).map_err(|source| {
        let reason = if source.kind() == io::ErrorKind::NotFound {
            "does not exist".to_string()
        } else {
            format!("cannot inspect: {source}")
        };
        VolumeError::InvalidMountPoint {
            path: mount_point.to_path_buf(),
            reason,
        }
    })?;

    if !metadata.is_dir() {
        return Err(VolumeError::InvalidMountPoint {
            path: mount_point.to_path_buf(),
            reason: "is not a directory".to_string(),
        });
    }

    let mut entries = fs::read_dir(mount_point).map_err(|source| VolumeError::Io {
        context: format!("reading directory `{}`", mount_point.display()),
        source,
    })?;

    if entries.next().is_some() {
        return Err(VolumeError::InvalidMountPoint {
            path: mount_point.to_path_buf(),
            reason: "is not empty".to_string(),
        });
    }

    Ok(())
}

/// Proves the volume is usable by the current, unprivileged user: creates and removes a small
/// probe file at its root.
fn verify_writable(mount_point: &Path) -> Result<(), VolumeError> {
    let probe = mount_point.join(".excise-harness-readiness-probe");
    fs::write(&probe, b"excise-harness readiness probe\n").map_err(|source| VolumeError::Io {
        context: format!("verifying that `{}` is writable", mount_point.display()),
        source,
    })?;
    fs::remove_file(&probe).map_err(|source| VolumeError::Io {
        context: format!("removing readiness probe from `{}`", mount_point.display()),
        source,
    })
}

/// The image file for `spec` inside `work_dir`: named for the label, with the extension the
/// platform's image format uses.
fn image_path_for(spec: &VolumeSpec, work_dir: &Path) -> PathBuf {
    let extension = match current_platform() {
        Platform::MacOs => "sparseimage",
        Platform::Windows => "vhd",
        Platform::Linux | Platform::Other(_) => "img",
    };
    work_dir.join(format!("{}.{extension}", spec.label))
}

/// Refuses to reuse an image path that is already taken: whatever is there is not ours to
/// overwrite, and not ours to remove if creating the image fails.
fn ensure_absent(image_path: &Path) -> Result<(), VolumeError> {
    match fs::symlink_metadata(image_path) {
        Ok(_) => Err(VolumeError::Io {
            context: format!(
                "the image path `{}` already exists; refusing to overwrite it",
                image_path.display()
            ),
            source: io::Error::from(io::ErrorKind::AlreadyExists),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(VolumeError::Io {
            context: format!("inspecting `{}`", image_path.display()),
            source,
        }),
    }
}

/// Converts a path to a process argument without lossy conversion.
fn path_arg(path: &Path) -> OsString {
    path.as_os_str().to_os_string()
}

/// The three platforms this crate implements, plus a catch-all so `Unsupported` can name what it
/// is not supported on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Platform {
    MacOs,
    Linux,
    Windows,
    Other(&'static str),
}

/// Selects the implementation at run time (rather than with `#[cfg(target_os = "...")]` on the
/// platform functions) so that compiling on any one host still type-checks every platform's code.
fn current_platform() -> Platform {
    if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "windows") {
        Platform::Windows
    } else {
        Platform::Other(env::consts::OS)
    }
}

/// How often [`run`] polls a child process for completion.
const POLL_INTERVAL: Duration = Duration::from_millis(20);
/// Timeout for a create/attach/format helper command.
const SETUP_TIMEOUT: Duration = Duration::from_secs(120);
/// Timeout for a detach helper command.
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(60);

/// One helper command this crate may run, with the timeout that applies to it. A plain data
/// description: building one is a pure function, so every platform's argument list can be
/// unit-tested without spawning anything.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cmd {
    program: &'static str,
    args: Vec<OsString>,
    timeout: Duration,
}

impl Cmd {
    fn new(program: &'static str, args: Vec<OsString>, timeout: Duration) -> Self {
        Self {
            program,
            args,
            timeout,
        }
    }
}

/// Runs `cmd` to completion or until its timeout, whichever comes first.
///
/// Spawns the child with piped stdout/stderr, drains both on background threads so a full pipe
/// cannot deadlock the wait, and polls `try_wait` every [`POLL_INTERVAL`]. Past the deadline, the
/// child is killed and [`VolumeError::Command`] reports a timeout. On a non-zero exit,
/// [`VolumeError::Command`] carries the captured stderr. On success, returns captured stdout.
fn run(cmd: &Cmd) -> Result<String, VolumeError> {
    let mut child = Command::new(cmd.program)
        .args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| VolumeError::Io {
            context: format!("spawning `{}`", cmd.program),
            source,
        })?;

    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());

    let deadline = Instant::now() + cmd.timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(source) => {
                return Err(VolumeError::Io {
                    context: format!("waiting for `{}`", cmd.program),
                    source,
                });
            }
        }
        if Instant::now() >= deadline {
            break None;
        }
        thread::sleep(POLL_INTERVAL);
    };

    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        let stderr = stderr_reader.join().unwrap_or_default();
        return Err(VolumeError::Command {
            program: cmd.program.to_string(),
            args: display_args(&cmd.args),
            status: format!("timed out after {}s", cmd.timeout.as_secs()),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        });
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let stdout = String::from_utf8_lossy(&stdout).into_owned();

    if status.success() {
        Ok(stdout)
    } else {
        Err(VolumeError::Command {
            program: cmd.program.to_string(),
            args: display_args(&cmd.args),
            status: status.to_string(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        })
    }
}

/// Spawns a thread that reads `pipe` to completion, so a child process's output can never fill
/// an unread pipe and deadlock the process waiting on it.
fn drain<R>(pipe: Option<R>) -> thread::JoinHandle<Vec<u8>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

fn display_args(args: &[OsString]) -> Vec<String> {
    args.iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// macOS: `hdiutil create -fs APFS -type SPARSE` then `hdiutil attach -mountpoint`. No root
/// needed. `hdiutil` warns that these forms are deprecated in favor of `diskutil image`; the
/// warnings go to stderr and are not failures.
mod macos {
    use std::{ffi::OsString, fs, path::Path};

    use super::{
        Cmd, SETUP_TIMEOUT, TEARDOWN_TIMEOUT, Volume, VolumeError, VolumeSpec, ensure_absent,
        image_path_for, path_arg, run, verify_writable,
    };

    pub(super) fn attach(
        spec: &VolumeSpec,
        work_dir: &Path,
        mount_point: &Path,
    ) -> Result<Volume, VolumeError> {
        let image_path = image_path_for(spec, work_dir);
        ensure_absent(&image_path)?;

        if let Err(err) = run(&create_cmd(spec, &image_path)) {
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        let stdout = match run(&attach_cmd(&image_path, mount_point)) {
            Ok(stdout) => stdout,
            Err(err) => {
                let _ = fs::remove_file(&image_path);
                return Err(err);
            }
        };

        let whole_disk = parse_whole_disk(&stdout);

        if let Err(err) = verify_writable(mount_point) {
            let _ = detach(mount_point, whole_disk.as_deref());
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        Ok(Volume {
            mount_point: mount_point.to_path_buf(),
            image_path,
            spec: spec.clone(),
            macos_whole_disk: whole_disk,
            armed: true,
        })
    }

    pub(super) fn detach(mount_point: &Path, whole_disk: Option<&str>) -> Result<(), VolumeError> {
        if run(&detach_cmd(mount_point)).is_ok() {
            return Ok(());
        }
        let forced = run(&detach_force_cmd(mount_point));
        if forced.is_ok() {
            return Ok(());
        }

        let Some(device) = whole_disk else {
            return forced.map(|_| ());
        };

        let device_path = Path::new(device);
        if run(&detach_cmd(device_path)).is_ok() {
            return Ok(());
        }
        run(&detach_force_cmd(device_path)).map(|_| ())
    }

    pub(super) fn create_cmd(spec: &VolumeSpec, image_path: &Path) -> Cmd {
        Cmd::new(
            "hdiutil",
            vec![
                OsString::from("create"),
                OsString::from("-size"),
                OsString::from(format!("{}m", spec.size_mib)),
                OsString::from("-fs"),
                OsString::from("APFS"),
                OsString::from("-volname"),
                OsString::from(spec.label.clone()),
                OsString::from("-type"),
                OsString::from("SPARSE"),
                path_arg(image_path),
            ],
            SETUP_TIMEOUT,
        )
    }

    pub(super) fn attach_cmd(image_path: &Path, mount_point: &Path) -> Cmd {
        Cmd::new(
            "hdiutil",
            vec![
                OsString::from("attach"),
                OsString::from("-nobrowse"),
                OsString::from("-noverify"),
                OsString::from("-noautoopen"),
                OsString::from("-mountpoint"),
                path_arg(mount_point),
                path_arg(image_path),
            ],
            SETUP_TIMEOUT,
        )
    }

    pub(super) fn detach_cmd(target: &Path) -> Cmd {
        Cmd::new(
            "hdiutil",
            vec![OsString::from("detach"), path_arg(target)],
            TEARDOWN_TIMEOUT,
        )
    }

    pub(super) fn detach_force_cmd(target: &Path) -> Cmd {
        Cmd::new(
            "hdiutil",
            vec![
                OsString::from("detach"),
                OsString::from("-force"),
                path_arg(target),
            ],
            TEARDOWN_TIMEOUT,
        )
    }

    /// Parses the whole-disk device node (e.g. `/dev/disk4`) from the first line of `hdiutil
    /// attach`'s textual output, for use as a detach fallback if the mount point stops resolving.
    pub(super) fn parse_whole_disk(attach_stdout: &str) -> Option<String> {
        let first_line = attach_stdout.lines().next()?;
        let device = first_line.split_whitespace().next()?;
        if device.starts_with("/dev/disk") {
            Some(device.to_string())
        } else {
            None
        }
    }
}

/// Linux: a plain file formatted with `mkfs.ext4` and loop-mounted. Needs root; written from the
/// tools' documentation and never run by its author.
mod linux {
    use std::{ffi::OsString, fs, path::Path, time::Duration};

    use super::{
        Cmd, SETUP_TIMEOUT, TEARDOWN_TIMEOUT, Volume, VolumeError, VolumeSpec, ensure_absent,
        image_path_for, path_arg, run, verify_writable,
    };

    pub(super) fn attach(
        spec: &VolumeSpec,
        work_dir: &Path,
        mount_point: &Path,
    ) -> Result<Volume, VolumeError> {
        let image_path = image_path_for(spec, work_dir);
        ensure_absent(&image_path)?;

        if let Err(err) = create_image_file(&image_path, spec.size_mib) {
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        let uid = match current_id("-u") {
            Ok(uid) => uid,
            Err(err) => {
                let _ = fs::remove_file(&image_path);
                return Err(err);
            }
        };
        let gid = match current_id("-g") {
            Ok(gid) => gid,
            Err(err) => {
                let _ = fs::remove_file(&image_path);
                return Err(err);
            }
        };

        if let Err(err) = run(&mkfs_cmd(spec, &image_path, uid)) {
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        if let Err(err) = run(&mount_cmd(&image_path, mount_point, uid)) {
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        if let Err(err) = run(&chown_cmd(mount_point, uid, gid)) {
            let _ = detach(mount_point);
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        if let Err(err) = verify_writable(mount_point) {
            let _ = detach(mount_point);
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        Ok(Volume {
            mount_point: mount_point.to_path_buf(),
            image_path,
            spec: spec.clone(),
            macos_whole_disk: None,
            armed: true,
        })
    }

    pub(super) fn detach(mount_point: &Path) -> Result<(), VolumeError> {
        let uid = current_id("-u")?;
        if run(&umount_cmd(mount_point, uid)).is_ok() {
            return Ok(());
        }
        run(&umount_lazy_cmd(mount_point, uid)).map(|_| ())
    }

    fn create_image_file(image_path: &Path, size_mib: u32) -> Result<(), VolumeError> {
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(image_path)
            .map_err(|source| VolumeError::Io {
                context: format!("creating image file `{}`", image_path.display()),
                source,
            })?;
        file.set_len(u64::from(size_mib) * 1024 * 1024)
            .map_err(|source| VolumeError::Io {
                context: format!("sizing image file `{}`", image_path.display()),
                source,
            })
    }

    fn current_id(flag: &'static str) -> Result<u32, VolumeError> {
        let stdout = run(&Cmd::new("id", vec![OsString::from(flag)], SETUP_TIMEOUT))?;
        let trimmed = stdout.trim();
        trimmed
            .parse::<u32>()
            .map_err(|_source| VolumeError::Command {
                program: "id".to_string(),
                args: vec![flag.to_string()],
                status: "unexpected output".to_string(),
                stderr: format!("could not parse `{trimmed}` as a numeric id"),
            })
    }

    pub(super) fn is_root(uid: u32) -> bool {
        uid == 0
    }

    /// Wraps `program`/`args` with a non-interactive `sudo -n` unless `uid` is already root.
    pub(super) fn privileged(
        uid: u32,
        program: &'static str,
        args: Vec<OsString>,
        timeout: Duration,
    ) -> Cmd {
        if is_root(uid) {
            Cmd::new(program, args, timeout)
        } else {
            let mut full = Vec::with_capacity(args.len() + 2);
            full.push(OsString::from("-n"));
            full.push(OsString::from(program));
            full.extend(args);
            Cmd::new("sudo", full, timeout)
        }
    }

    pub(super) fn mkfs_cmd(spec: &VolumeSpec, image_path: &Path, uid: u32) -> Cmd {
        privileged(
            uid,
            "mkfs.ext4",
            vec![
                OsString::from("-q"),
                OsString::from("-F"),
                OsString::from("-L"),
                OsString::from(spec.label.clone()),
                path_arg(image_path),
            ],
            SETUP_TIMEOUT,
        )
    }

    pub(super) fn mount_cmd(image_path: &Path, mount_point: &Path, uid: u32) -> Cmd {
        privileged(
            uid,
            "mount",
            vec![
                OsString::from("-o"),
                OsString::from("loop,nodev,nosuid"),
                path_arg(image_path),
                path_arg(mount_point),
            ],
            SETUP_TIMEOUT,
        )
    }

    pub(super) fn chown_cmd(mount_point: &Path, uid: u32, gid: u32) -> Cmd {
        privileged(
            uid,
            "chown",
            vec![
                OsString::from(format!("{uid}:{gid}")),
                path_arg(mount_point),
            ],
            SETUP_TIMEOUT,
        )
    }

    pub(super) fn umount_cmd(mount_point: &Path, uid: u32) -> Cmd {
        privileged(uid, "umount", vec![path_arg(mount_point)], TEARDOWN_TIMEOUT)
    }

    pub(super) fn umount_lazy_cmd(mount_point: &Path, uid: u32) -> Cmd {
        privileged(
            uid,
            "umount",
            vec![OsString::from("-l"), path_arg(mount_point)],
            TEARDOWN_TIMEOUT,
        )
    }
}

/// Windows: a `diskpart` script creates and formats a VHD and assigns it to the mount folder.
/// Needs an elevated (Administrator) process. Written from documentation and never run by its
/// author; success is judged by exit status and a writability probe rather than a mount-specific
/// API, since diskpart's textual output is not documented as stable or localization-safe.
mod windows {
    use std::{ffi::OsString, fs, path::Path};

    use super::{
        Cmd, SETUP_TIMEOUT, TEARDOWN_TIMEOUT, Volume, VolumeError, VolumeSpec, ensure_absent,
        image_path_for, path_arg, run, verify_writable,
    };

    pub(super) fn attach(
        spec: &VolumeSpec,
        work_dir: &Path,
        mount_point: &Path,
    ) -> Result<Volume, VolumeError> {
        let image_path = image_path_for(spec, work_dir);
        ensure_absent(&image_path)?;
        let script_path = work_dir.join(format!("{}-attach.diskpart.txt", spec.label));

        let script = attach_script(spec, &image_path, mount_point)?;
        write_script(&script_path, &script)?;

        let run_result = run(&diskpart_cmd(&script_path, SETUP_TIMEOUT));
        let _ = fs::remove_file(&script_path);

        if let Err(err) = run_result {
            let _ = detach(&image_path);
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        if let Err(err) = verify_writable(mount_point) {
            let _ = detach(&image_path);
            let _ = fs::remove_file(&image_path);
            return Err(err);
        }

        Ok(Volume {
            mount_point: mount_point.to_path_buf(),
            image_path,
            spec: spec.clone(),
            macos_whole_disk: None,
            armed: true,
        })
    }

    pub(super) fn detach(image_path: &Path) -> Result<(), VolumeError> {
        let work_dir = image_path.parent().unwrap_or_else(|| Path::new("."));
        let script_path = work_dir.join("excise-harness-detach.diskpart.txt");
        let script = detach_script(image_path)?;
        write_script(&script_path, &script)?;
        let result = run(&diskpart_cmd(&script_path, TEARDOWN_TIMEOUT));
        let _ = fs::remove_file(&script_path);
        result.map(|_| ())
    }

    fn write_script(path: &Path, contents: &str) -> Result<(), VolumeError> {
        fs::write(path, contents).map_err(|source| VolumeError::Io {
            context: format!("writing diskpart script `{}`", path.display()),
            source,
        })
    }

    pub(super) fn diskpart_cmd(script_path: &Path, timeout: std::time::Duration) -> Cmd {
        Cmd::new(
            "diskpart",
            vec![OsString::from("/s"), path_arg(script_path)],
            timeout,
        )
    }

    /// Which argument a path came from, so a rejected path can be reported against the right
    /// field: [`VolumeError::InvalidMountPoint`] for `mount_point`, [`VolumeError::InvalidSpec`]
    /// for the image path (derived from `work_dir`).
    #[derive(Debug, Clone, Copy)]
    enum PathRole {
        MountPoint,
        Image,
    }

    /// Diskpart scripts have no escape for `"` and are line-oriented, so a path containing either
    /// cannot be expressed. Rejects those up front rather than producing a corrupt script.
    fn script_path_text(path: &Path, role: PathRole) -> Result<String, VolumeError> {
        let text = path.to_string_lossy().into_owned();
        if text.contains('"') || text.contains('\n') || text.contains('\r') {
            let reason =
                "diskpart scripts cannot represent a `\"` or a newline in a path".to_string();
            return Err(match role {
                PathRole::MountPoint => VolumeError::InvalidMountPoint {
                    path: path.to_path_buf(),
                    reason,
                },
                PathRole::Image => VolumeError::InvalidSpec {
                    reason: format!("work_dir path `{}`: {reason}", path.display()),
                },
            });
        }
        Ok(text)
    }

    pub(super) fn attach_script(
        spec: &VolumeSpec,
        image_path: &Path,
        mount_point: &Path,
    ) -> Result<String, VolumeError> {
        let image = script_path_text(image_path, PathRole::Image)?;
        let mount = script_path_text(mount_point, PathRole::MountPoint)?;
        Ok(format!(
            "create vdisk file=\"{image}\" maximum={size} type=expandable\r\n\
            select vdisk file=\"{image}\"\r\n\
            attach vdisk\r\n\
            create partition primary\r\n\
            format fs=ntfs quick label={label}\r\n\
            assign mount=\"{mount}\"\r\n",
            size = spec.size_mib,
            label = spec.label,
        ))
    }

    pub(super) fn detach_script(image_path: &Path) -> Result<String, VolumeError> {
        let image = script_path_text(image_path, PathRole::Image)?;
        Ok(format!("select vdisk file=\"{image}\"\r\ndetach vdisk\r\n"))
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::OsString,
        fs,
        path::{Path, PathBuf},
    };

    #[cfg(target_os = "macos")]
    use super::{Cmd, SETUP_TIMEOUT, TEARDOWN_TIMEOUT, ensure_absent, path_arg, run};
    use super::{
        MAX_LABEL_LEN, MAX_SIZE_MIB, MIN_SIZE_MIB, PrivilegedOptIn, Volume, VolumeError,
        VolumeSpec, image_path_for, linux, macos, opt_in_from_value, windows,
    };

    #[test]
    fn only_the_exact_value_one_opts_in() {
        assert!(opt_in_from_value(Some("1")).is_some());
        for other in [
            None,
            Some("0"),
            Some(""),
            Some("true"),
            Some(" 1"),
            Some("1 "),
            Some("11"),
        ] {
            assert!(
                opt_in_from_value(other).is_none(),
                "{other:?} must not opt in"
            );
        }
    }

    #[test]
    fn a_spec_accepts_exactly_the_supported_sizes() {
        for size in [MIN_SIZE_MIB, 16, MAX_SIZE_MIB] {
            assert!(VolumeSpec::new(size, "Ok").is_ok(), "{size} MiB");
        }
        for size in [0, MIN_SIZE_MIB - 1, MAX_SIZE_MIB + 1] {
            assert!(
                matches!(
                    VolumeSpec::new(size, "Ok"),
                    Err(VolumeError::InvalidSpec { .. })
                ),
                "{size} MiB"
            );
        }
    }

    #[test]
    fn a_label_is_short_ascii_that_starts_with_a_letter() {
        let longest = "A".repeat(MAX_LABEL_LEN);
        for good in ["A", "ab-cd_12", longest.as_str()] {
            assert!(VolumeSpec::new(MIN_SIZE_MIB, good).is_ok(), "{good}");
        }
        let too_long = "A".repeat(MAX_LABEL_LEN + 1);
        for bad in [
            "",
            "1abc",
            "_abc",
            "-abc",
            "abc def",
            "caf\u{e9}",
            "a/b",
            "a\"b",
            too_long.as_str(),
        ] {
            assert!(
                matches!(
                    VolumeSpec::new(MIN_SIZE_MIB, bad),
                    Err(VolumeError::InvalidSpec { .. })
                ),
                "{bad:?}"
            );
        }
    }

    /// Attaching at an unusable mount point fails before anything is created in the work
    /// directory.
    fn assert_refused_without_side_effects(mount_point: &Path) {
        let work_dir = tempfile::tempdir().expect("a work directory");
        let spec = VolumeSpec::new(MIN_SIZE_MIB, "PREATT").expect("a valid spec");
        let result = Volume::attach(
            PrivilegedOptIn::granted(),
            &spec,
            work_dir.path(),
            mount_point,
        );
        assert!(
            matches!(result, Err(VolumeError::InvalidMountPoint { .. })),
            "{result:?}"
        );
        assert!(
            fs::read_dir(work_dir.path())
                .expect("list")
                .next()
                .is_none(),
            "no image may be created before the mount point is validated"
        );
    }

    #[test]
    fn a_missing_path_a_file_and_a_non_empty_directory_are_not_mount_points() {
        let holder = tempfile::tempdir().expect("a directory");
        assert_refused_without_side_effects(&holder.path().join("missing"));

        let file = holder.path().join("file");
        fs::write(&file, b"not a directory").expect("write");
        assert_refused_without_side_effects(&file);

        let full = holder.path().join("full");
        fs::create_dir(&full).expect("mkdir");
        fs::write(full.join("inside"), b"pre-existing").expect("write");
        assert_refused_without_side_effects(&full);
    }

    #[test]
    fn attach_never_overwrites_or_removes_an_image_that_is_already_there() {
        let work_dir = tempfile::tempdir().expect("a work directory");
        let mount_point = tempfile::tempdir().expect("a mount point");
        let spec = VolumeSpec::new(MIN_SIZE_MIB, "PREEXIST").expect("a valid spec");
        let image = image_path_for(&spec, work_dir.path());
        fs::write(&image, b"precious").expect("write");

        let result = Volume::attach(
            PrivilegedOptIn::granted(),
            &spec,
            work_dir.path(),
            mount_point.path(),
        );
        assert!(matches!(result, Err(VolumeError::Io { .. })), "{result:?}");
        assert_eq!(fs::read(&image).expect("read"), b"precious");
        assert!(
            fs::read_dir(mount_point.path())
                .expect("list")
                .next()
                .is_none()
        );
    }

    #[test]
    fn macos_finds_the_whole_disk_device_on_the_first_line_of_the_attach_output() {
        let stdout = "/dev/disk4          \tGUID_partition_scheme          \t\n\
                      /dev/disk4s1        \tApple_APFS                     \t\n\
                      /dev/disk5s1        \t41504653-0000-11AA-AA11-0030654\t/private/tmp/x/mnt\n";
        assert_eq!(
            macos::parse_whole_disk(stdout),
            Some("/dev/disk4".to_owned())
        );
        assert_eq!(macos::parse_whole_disk(""), None);
        assert_eq!(macos::parse_whole_disk("not a device line\n"), None);
    }

    #[test]
    fn linux_commands_run_directly_as_root_and_through_non_interactive_sudo_otherwise() {
        let target = Path::new("/mnt/point");
        let as_root = linux::umount_cmd(target, 0);
        assert_eq!(as_root.program, "umount");
        assert_eq!(as_root.args, vec![OsString::from("/mnt/point")]);

        let as_user = linux::umount_cmd(target, 1000);
        assert_eq!(as_user.program, "sudo");
        assert_eq!(
            as_user.args,
            vec![
                OsString::from("-n"),
                OsString::from("umount"),
                OsString::from("/mnt/point")
            ]
        );
    }

    #[test]
    fn diskpart_scripts_quote_paths_with_spaces() -> Result<(), VolumeError> {
        let spec = VolumeSpec::new(MIN_SIZE_MIB, "LBL")?;
        let image = PathBuf::from(r"C:\Users\a b\LBL.vhd");
        let mount = PathBuf::from(r"C:\mount point");

        let attach = windows::attach_script(&spec, &image, &mount)?;
        assert!(attach.contains(r#"create vdisk file="C:\Users\a b\LBL.vhd" maximum=8"#));
        assert!(attach.contains(r#"assign mount="C:\mount point""#));
        let detach = windows::detach_script(&image)?;
        assert!(detach.contains(r#"select vdisk file="C:\Users\a b\LBL.vhd""#));
        assert!(detach.contains("detach vdisk"));
        Ok(())
    }

    #[test]
    fn diskpart_scripts_refuse_paths_they_cannot_express() -> Result<(), VolumeError> {
        let spec = VolumeSpec::new(MIN_SIZE_MIB, "LBL")?;
        let image = PathBuf::from(r"C:\work\LBL.vhd");
        let quoted = PathBuf::from("C:\\mount\"point");
        assert!(matches!(
            windows::attach_script(&spec, &image, &quoted),
            Err(VolumeError::InvalidMountPoint { .. })
        ));
        let broken_line = PathBuf::from("C:\\work\nLBL.vhd");
        assert!(matches!(
            windows::attach_script(&spec, &broken_line, &PathBuf::from(r"C:\mount")),
            Err(VolumeError::InvalidSpec { .. })
        ));
        Ok(())
    }

    // --------------------------------------------------------- macOS integration (opt-in) ------

    /// The one macOS integration test: a real `hdiutil` attach/fill/detach lifecycle, gated on
    /// [`PrivilegedOptIn::from_env`] so it never runs unless a caller explicitly asked for it.
    ///
    /// Cfg-gated to macOS (rather than only runtime-gated) because it uses
    /// `std::os::unix::fs::MetadataExt`, which does not exist off Unix, and because it drives the
    /// real `hdiutil` binary.
    #[cfg(target_os = "macos")]
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one linear real-hdiutil lifecycle covering attach/fill/detach/drop/failure, \
                  as specified; splitting it would multiply slow real attach/detach calls"
    )]
    fn macos_privileged_lifecycle() -> Result<(), Box<dyn std::error::Error>> {
        use std::{io, os::unix::fs::MetadataExt};

        let Some(opt_in) = PrivilegedOptIn::from_env() else {
            eprintln!("skipped: EXCISE_HARNESS_PRIVILEGED=1 not set");
            return Ok(());
        };

        let spec = VolumeSpec::new(16, "H2ITEST")?;

        // (a) attach puts the mount point on a different device than its parent.
        let work_dir = tempfile::tempdir()?;
        let mount_point = tempfile::tempdir()?;
        let pre_dev = fs::metadata(mount_point.path())?.dev();

        let attach_start = std::time::Instant::now();
        let volume = Volume::attach(opt_in, &spec, work_dir.path(), mount_point.path())?;
        let attach_elapsed = attach_start.elapsed();

        let attached_dev = fs::metadata(volume.mount_point())?.dev();
        assert_ne!(
            pre_dev, attached_dev,
            "mount point must be on a different device once attached"
        );

        let image_path = volume.image_path().to_path_buf();
        assert!(image_path.exists(), "image file must exist while attached");

        // (b) the size limit is real: writing past it fails with an out-of-space error, and the
        // total written is within [50%, 100%] of size_mib. Bounded at size_mib + 8 MiB.
        let chunk = vec![0_u8; 1024 * 1024];
        let max_bytes = u64::from(spec.size_mib + 8) * 1024 * 1024;
        let mut total_written: u64 = 0;
        let mut index: u32 = 0;
        loop {
            let path = volume.mount_point().join(format!("fill-{index}.bin"));
            match fs::write(&path, &chunk) {
                Ok(()) => {
                    total_written += chunk.len() as u64;
                    index += 1;
                    assert!(
                        total_written <= max_bytes,
                        "wrote {total_written} bytes without hitting an out-of-space error"
                    );
                }
                Err(err)
                    if err.kind() == io::ErrorKind::StorageFull
                        || err.raw_os_error() == Some(28) =>
                {
                    break;
                }
                Err(err) => return Err(Box::new(err)),
            }
        }

        let written_mib = total_written / (1024 * 1024);
        let min_expected_mib = u64::from(spec.size_mib) / 2;
        assert!(
            written_mib >= min_expected_mib && written_mib <= u64::from(spec.size_mib),
            "wrote {written_mib} MiB before running out of space, expected between \
             {min_expected_mib} and {} MiB for a {} MiB volume",
            spec.size_mib,
            spec.size_mib,
        );

        // (c) detach puts the mount point back on its parent's device and removes the image.
        let detach_start = std::time::Instant::now();
        volume.detach()?;
        let detach_elapsed = detach_start.elapsed();

        let post_dev = fs::metadata(mount_point.path())?.dev();
        assert_eq!(
            pre_dev, post_dev,
            "mount point must be back on its parent device after detach"
        );
        assert!(!image_path.exists(), "image file must be gone after detach");

        eprintln!(
            "macos_privileged_lifecycle: attach={attach_elapsed:?} detach={detach_elapsed:?} \
             wrote={written_mib}MiB of {}MiB requested",
            spec.size_mib
        );

        // (d) Drop without an explicit detach also cleans up.
        let drop_work_dir = tempfile::tempdir()?;
        let drop_mount_point = tempfile::tempdir()?;
        let drop_spec = VolumeSpec::new(16, "H2DROP")?;
        let drop_pre_dev = fs::metadata(drop_mount_point.path())?.dev();

        let drop_volume = Volume::attach(
            opt_in,
            &drop_spec,
            drop_work_dir.path(),
            drop_mount_point.path(),
        )?;
        let drop_image_path = drop_volume.image_path().to_path_buf();
        assert_ne!(drop_pre_dev, fs::metadata(drop_volume.mount_point())?.dev());
        drop(drop_volume);

        let drop_post_dev = fs::metadata(drop_mount_point.path())?.dev();
        assert_eq!(
            drop_pre_dev, drop_post_dev,
            "Drop must detach without an explicit call"
        );
        assert!(!drop_image_path.exists(), "Drop must remove the image file");

        // (e) a failure path (a work_dir that does not exist) leaves nothing behind.
        let bad_work_dir = tempfile::tempdir()?;
        let nonexistent_work_dir = bad_work_dir.path().join("does-not-exist");
        let fail_mount_point = tempfile::tempdir()?;
        let fail_spec = VolumeSpec::new(16, "H2FAIL")?;

        let attach_result = Volume::attach(
            opt_in,
            &fail_spec,
            &nonexistent_work_dir,
            fail_mount_point.path(),
        );
        assert!(
            attach_result.is_err(),
            "attach with a nonexistent work_dir must fail"
        );

        let mut fail_mount_entries = fs::read_dir(fail_mount_point.path())?;
        assert!(
            fail_mount_entries.next().is_none(),
            "mount point must remain untouched after a failed attach"
        );

        let info = std::process::Command::new("hdiutil").arg("info").output()?;
        let info_stdout = String::from_utf8_lossy(&info.stdout);
        let fail_mount_str = fail_mount_point.path().display().to_string();
        assert!(
            !info_stdout.contains(&fail_mount_str),
            "hdiutil must not report the failed mount point as attached"
        );

        Ok(())
    }

    /// A real image that `hdiutil` attached at a mount point with owners honored or ignored as
    /// asked, which [`Volume::attach`] does not offer: it takes what the system does by default
    /// for a disk image. Detached (forced if need be) and removed when dropped.
    #[cfg(target_os = "macos")]
    struct ImageWithOwners {
        mount_point: PathBuf,
        image: PathBuf,
    }

    #[cfg(target_os = "macos")]
    impl ImageWithOwners {
        fn attach(
            owners: &str,
            spec: &VolumeSpec,
            work_dir: &Path,
            mount_point: &Path,
        ) -> Result<Self, VolumeError> {
            let image = image_path_for(spec, work_dir);
            ensure_absent(&image)?;
            run(&macos::create_cmd(spec, &image))?;
            let attach = Cmd::new(
                "hdiutil",
                vec![
                    OsString::from("attach"),
                    OsString::from("-nobrowse"),
                    OsString::from("-noverify"),
                    OsString::from("-noautoopen"),
                    OsString::from("-owners"),
                    OsString::from(owners),
                    OsString::from("-mountpoint"),
                    path_arg(mount_point),
                    path_arg(&image),
                ],
                SETUP_TIMEOUT,
            );
            if let Err(error) = run(&attach) {
                let _ = fs::remove_file(&image);
                return Err(error);
            }
            Ok(Self {
                mount_point: mount_point.to_path_buf(),
                image,
            })
        }
    }

    #[cfg(target_os = "macos")]
    impl Drop for ImageWithOwners {
        fn drop(&mut self) {
            let _ = macos::detach(&self.mount_point, None);
            let _ = fs::remove_file(&self.image);
        }
    }

    /// The one test of what the read-only soak asks of its scratch directory that a stand-in cannot
    /// show: a real volume mounted without owners ("Ignore ownership", `MNT_IGNORE_OWNERSHIP`) is
    /// refused as a scratch directory, whatever mode its directories have, and one mounted with
    /// owners is not refused for that. `mount(8)`, which decodes the flags of the same `statfs`
    /// itself, is the oracle for which of the two a volume is. Gated on
    /// [`PrivilegedOptIn::from_env`], like the lifecycle test above.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_privileged_a_volume_mounted_without_owners_is_refused_as_a_scratch_directory()
    -> Result<(), Box<dyn std::error::Error>> {
        use crate::safety::{Untrusted, check_private_directory};

        if PrivilegedOptIn::from_env().is_none() {
            eprintln!("skipped: EXCISE_HARNESS_PRIVILEGED=1 not set");
            return Ok(());
        }
        let work_dir = tempfile::tempdir()?;

        let mut seen_without_owners = false;
        for (owners, label) in [("off", "OWNSOFF"), ("on", "OWNSON")] {
            let mount_point = tempfile::tempdir()?;
            let spec = VolumeSpec::new(8, label)?;
            let volume =
                ImageWithOwners::attach(owners, &spec, work_dir.path(), mount_point.path())?;
            let canonical = fs::canonicalize(mount_point.path())?;

            let mounted = run(&Cmd::new("mount", Vec::new(), TEARDOWN_TIMEOUT))?;
            let line = mounted
                .lines()
                .find(|line| line.contains(&format!(" on {} (", canonical.display())))
                .ok_or("the volume is not in the list that mount prints")?
                .to_owned();
            let without_owners = line.contains("noowners");
            eprintln!("hdiutil attach -owners {owners}: {line}");
            assert!(
                owners != "off" || without_owners,
                "a volume attached with -owners off must say noowners: {line}"
            );
            seen_without_owners |= without_owners;

            // A directory below the root of the volume, made by its owner: whatever the volume
            // says about ownership, the mode of this one is closed to the group and everybody.
            let below = mount_point.path().join("scratch");
            fs::create_dir(&below)?;
            for asked in [&canonical, &below] {
                let verdict = check_private_directory(asked);
                match verdict {
                    Err(refusal) if without_owners => assert_eq!(
                        refusal.why,
                        Untrusted::IgnoresOwnership,
                        "{}: {refusal}",
                        asked.display()
                    ),
                    Ok(()) if without_owners => panic!(
                        "{} is on a volume that ignores ownership, and it was accepted: {line}",
                        asked.display()
                    ),
                    Err(refusal) => assert_ne!(
                        refusal.why,
                        Untrusted::IgnoresOwnership,
                        "a volume that honors owners was refused for ignoring them: {refusal}: {line}"
                    ),
                    Ok(()) => {}
                }
            }
            drop(volume);
        }
        assert!(seen_without_owners, "no volume without owners was seen");
        Ok(())
    }
}
