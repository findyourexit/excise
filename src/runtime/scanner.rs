mod task_queue;
mod task_spill;

#[cfg(windows)]
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
#[cfg(all(test, unix))]
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use cap_primitives::ambient_authority;
use cap_primitives::fs::{self as cap_fs};
use crossbeam_channel::Sender;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use task_queue::{DirectoryTask, TASK_QUEUE_PER_WORKER, TaskQueue};

use super::worker::{ScannedEntry, WorkerEvent, send_event};
use crate::model::UnscannedReason;
use crate::native_path::{NativeIdentity, identity_for};
use crate::temporary_storage::TemporaryStorage;

// The owner stages each batch and applies one entry per input poll. Keep batches
// bounded so backpressure stays small while the map is navigating.
const BATCH_SIZE: usize = 32;

#[derive(Clone, Debug)]
pub struct ScannerOptions {
    pub root: PathBuf,
    pub root_identity: Option<NativeIdentity>,
    pub threads: usize,
    pub cross_filesystems: bool,
    pub exclusions: Vec<String>,
    pub internal_paths: Vec<PathBuf>,
    pub temporary_storage: TemporaryStorage,
}

struct Exclusions {
    matcher: Gitignore,
    rules: Vec<(String, Gitignore)>,
    internal_paths: Vec<PathBuf>,
}

impl Exclusions {
    fn new(
        root: &Path,
        patterns: Vec<String>,
        internal_paths: Vec<PathBuf>,
    ) -> Result<Self, String> {
        if let Some(path) = internal_paths.iter().find(|path| !path.is_absolute()) {
            return Err(format!(
                "internal scanner path must be absolute: {}",
                path.to_string_lossy()
            ));
        }
        let matcher = crate::config::compile_exclusions(root, &patterns)
            .map_err(|error| error.to_string())?;
        let mut rules = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            let mut rule = GitignoreBuilder::new(root);
            rule.add_line(None, &pattern)
                .map_err(|error| error.to_string())?;
            rules.push((pattern, rule.build().map_err(|error| error.to_string())?));
        }
        Ok(Self {
            matcher,
            rules,
            internal_paths,
        })
    }

    fn is_internal(&self, path: &Path) -> bool {
        self.internal_paths
            .iter()
            .any(|internal_path| path == internal_path || path.starts_with(internal_path))
    }

    fn reason(&self, path: &Path, is_dir: bool) -> Option<String> {
        if self.is_internal(path) {
            return Some("Excise session state".to_string());
        }
        if !self
            .matcher
            .matched_path_or_any_parents(path, is_dir)
            .is_ignore()
        {
            return None;
        }
        self.rules
            .iter()
            .rev()
            .find(|(_, matcher)| {
                matcher
                    .matched_path_or_any_parents(path, is_dir)
                    .is_ignore()
            })
            .map_or_else(
                || Some("configured exclusion".to_string()),
                |(pattern, _)| Some(pattern.clone()),
            )
    }
}

pub fn spawn(
    options: ScannerOptions,
    sender: Sender<WorkerEvent>,
    cancelled: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, std::io::Error> {
    thread::Builder::new()
        .name("excise-scanner".to_string())
        .spawn(move || run(options, &sender, cancelled.as_ref()))
}

#[allow(clippy::too_many_lines)]
pub(super) fn run(options: ScannerOptions, sender: &Sender<WorkerEvent>, cancelled: &AtomicBool) {
    let ScannerOptions {
        root,
        root_identity,
        threads,
        cross_filesystems,
        exclusions: exclusion_patterns,
        internal_paths,
        temporary_storage,
    } = options;
    if let Err(message) = validate_scan_root(&root, root_identity.as_ref()) {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root.clone()),
                message,
            },
            cancelled,
        );
        let _ = send_event(
            sender,
            WorkerEvent::ScanFinished { cancelled: false },
            cancelled,
        );
        return;
    }
    let mut exclusions = match Exclusions::new(&root, exclusion_patterns, internal_paths) {
        Ok(exclusions) => exclusions,
        Err(message) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root),
                    message,
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
    };
    let root_filesystem = if cross_filesystems {
        None
    } else {
        match filesystem_key(&root) {
            Ok(key) => Some(key),
            Err(error) => {
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: Some(root.clone()),
                        message: format!("could not identify root filesystem: {error}"),
                    },
                    cancelled,
                );
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFinished { cancelled: false },
                    cancelled,
                );
                return;
            }
        }
    };
    let root_directory = match cap_fs::open_ambient_dir(&root, ambient_authority()) {
        Ok(directory) => directory,
        Err(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root.clone()),
                    message: format!("could not open scan root: {error}"),
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
    };
    if let Err(error) = validate_root_handle(&root_directory, root_identity.as_ref()) {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root.clone()),
                message: format!("could not validate opened scan root: {error}"),
            },
            cancelled,
        );
        let _ = send_event(
            sender,
            WorkerEvent::ScanFinished { cancelled: false },
            cancelled,
        );
        return;
    }
    let (queue, task_spill_path) = match TaskQueue::new(
        root.clone(),
        threads.saturating_mul(TASK_QUEUE_PER_WORKER).max(1),
        &temporary_storage,
    ) {
        Ok(queue) => queue,
        Err(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root.clone()),
                    message: format!("could not create private scanner task spill: {error}"),
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
    };
    if let Some(task_spill_path) = task_spill_path {
        if !task_spill_path.is_absolute() {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(root.clone()),
                    message: "scanner task spill path must be absolute".to_string(),
                },
                cancelled,
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: false },
                cancelled,
            );
            return;
        }
        exclusions.internal_paths.push(task_spill_path);
    }
    let root_invalid = AtomicBool::new(false);
    let scan_failed = AtomicBool::new(false);
    thread::scope(|scope| {
        for index in 0..threads {
            let worker_queue = &queue;
            let worker_sender = sender;
            let worker_cancelled = cancelled;
            let worker_failed = &scan_failed;
            let worker_root_invalid = &root_invalid;
            let worker_root_directory = &root_directory;
            let worker_exclusions = &exclusions;
            let worker_root_filesystem = root_filesystem.as_ref();
            let worker_root = &root;
            let worker_root_identity = root_identity.as_ref();
            if let Err(error) = thread::Builder::new()
                .name(format!("excise-scan-{index}"))
                .spawn_scoped(scope, move || {
                    scan_worker(
                        worker_queue,
                        worker_sender,
                        worker_cancelled,
                        worker_failed,
                        worker_root_invalid,
                        worker_root_directory,
                        worker_root,
                        worker_root_identity,
                        worker_exclusions,
                        worker_root_filesystem,
                        cross_filesystems,
                    );
                })
            {
                scan_failed.store(true, Ordering::Release);
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: Some(root.clone()),
                        message: format!("could not spawn scanner worker: {error}"),
                    },
                    cancelled,
                );
            }
        }
    });
    queue.cancel();
    let completion_delivery = AtomicBool::new(false);
    let _ = send_event(
        sender,
        WorkerEvent::ScanFinished {
            cancelled: cancelled.load(Ordering::Acquire)
                && !scan_failed.load(Ordering::Acquire)
                && !root_invalid.load(Ordering::Acquire),
        },
        &completion_delivery,
    );
}

struct ScanFrame {
    task: DirectoryTask,
    _directory: File,
    identity: Option<NativeIdentity>,
    entries: cap_fs::ReadDir,
    batch: Vec<ScannedEntry>,
    directories: Vec<DirectoryTask>,
}
#[allow(
    clippy::too_many_arguments,
    reason = "The worker receives its bounded queue, cancellation state, root identity, and scan policy explicitly."
)]
fn scan_worker(
    queue: &TaskQueue,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
    root_directory: &File,
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) {
    loop {
        let task = match queue.take(cancelled, failed, root_invalid) {
            Ok(Some(task)) => task,
            Ok(None) => return,
            Err(error) => {
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: None,
                        message: format!("could not read scanner task spill: {error}"),
                    },
                    cancelled,
                );
                failed.store(true, Ordering::Release);
                queue.cancel();
                return;
            }
        };
        let completed = scan_directory(
            task,
            queue,
            sender,
            cancelled,
            failed,
            root_invalid,
            root_directory,
            root,
            root_identity,
            exclusions,
            root_filesystem,
            cross_filesystems,
        );
        queue.complete();
        if !completed {
            if !root_invalid.load(Ordering::Acquire)
                && !failed.load(Ordering::Acquire)
                && !cancelled.load(Ordering::Acquire)
            {
                cancelled.store(true, Ordering::Release);
            }
            queue.cancel();
            return;
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn scan_directory(
    task: DirectoryTask,
    queue: &TaskQueue,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
    root_invalid: &AtomicBool,
    root_directory: &File,
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) -> bool {
    if cancelled.load(Ordering::Acquire)
        || failed.load(Ordering::Acquire)
        || root_invalid.load(Ordering::Acquire)
    {
        return false;
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return false;
    }
    let mut frame = match open_frame(root_directory, root, task) {
        Ok(frame) => frame,
        Err(error) => {
            let (task, error) = *error;
            report_directory_task_error(task.path, error, sender, cancelled);
            return true;
        }
    };
    if let Err(error) = validate_directory_task(root_directory, root, &frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
        return true;
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return false;
    }
    loop {
        if cancelled.load(Ordering::Acquire)
            || failed.load(Ordering::Acquire)
            || root_invalid.load(Ordering::Acquire)
        {
            return false;
        }
        if frame.batch.len() == BATCH_SIZE {
            if !flush_frame(&mut frame, queue, sender, cancelled, failed) {
                return false;
            }
            if let Err(error) = validate_directory_task(root_directory, root, &frame.task) {
                report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
                return true;
            }
        }
        match frame.entries.next() {
            Some(Ok(entry)) => {
                process_entry(
                    &mut frame,
                    &entry,
                    sender,
                    cancelled,
                    exclusions,
                    root_filesystem,
                    cross_filesystems,
                );
            }
            Some(Err(error)) => {
                let _ = send_event(
                    sender,
                    WorkerEvent::ScanFailed {
                        path: Some(frame.task.path.clone()),
                        message: error.to_string(),
                    },
                    cancelled,
                );
            }
            None => break,
        }
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return false;
    }
    if !frame.batch.is_empty() && !flush_frame(&mut frame, queue, sender, cancelled, failed) {
        return false;
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return false;
    }
    if let Err(error) = validate_directory_task(root_directory, root, &frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
        return true;
    }
    send_event(
        sender,
        WorkerEvent::ScanDirectoryComplete {
            path: frame.task.path,
            identity: frame.identity,
        },
        cancelled,
    )
}

fn validate_root_for_traversal(
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    root_invalid: &AtomicBool,
) -> bool {
    if root_invalid.load(Ordering::Acquire) {
        return false;
    }
    let Err(message) = validate_scan_root(root, root_identity) else {
        return true;
    };
    let message = if message == "scan root identity changed before scanning" {
        "scan root identity changed during traversal".to_string()
    } else {
        format!("scan root became invalid during traversal: {message}")
    };
    if root_invalid
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
    {
        let _ = send_event(
            sender,
            WorkerEvent::ScanFailed {
                path: Some(root.to_path_buf()),
                message,
            },
            cancelled,
        );
    }
    false
}

enum DirectoryTaskError {
    Replaced(String),
    Io(io::Error),
}

fn validate_directory_task(
    root_directory: &File,
    root: &Path,
    task: &DirectoryTask,
) -> Result<(), DirectoryTaskError> {
    let Some(expected) = task.identity.as_ref() else {
        return Ok(());
    };
    let directory = open_scan_directory(root_directory, root, &task.path)
        .map_err(|error| classify_directory_open_error(&task.path, error))?;
    let metadata = cap_fs::Metadata::from_file(&directory).map_err(DirectoryTaskError::Io)?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(DirectoryTaskError::Replaced(
            "scanner directory task was replaced by a symbolic link or non-directory".to_string(),
        ));
    }
    let actual = identity_from_entry_metadata(&metadata)
        .map_err(DirectoryTaskError::Io)?
        .ok_or_else(|| {
            DirectoryTaskError::Replaced(
                "scanner directory task identity is unavailable".to_string(),
            )
        })?;
    if !same_identity(expected, &actual) || actual.reparse_point {
        return Err(DirectoryTaskError::Replaced(
            "scanner directory task identity changed before traversal".to_string(),
        ));
    }
    #[cfg(test)]
    maybe_replace_after_validation(&task.path);
    Ok(())
}

#[cfg(all(test, unix))]
static VALIDATION_REPLACEMENT: std::sync::OnceLock<Mutex<Option<(PathBuf, PathBuf, PathBuf)>>> =
    std::sync::OnceLock::new();
#[cfg(all(test, unix))]
static BATCH_REPLACEMENT: std::sync::OnceLock<Mutex<Option<(PathBuf, PathBuf, PathBuf)>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn maybe_replace_after_validation(path: &Path) {
    #[cfg(unix)]
    {
        let replacement = VALIDATION_REPLACEMENT.get_or_init(|| Mutex::new(None));
        let Some((expected_path, displaced_path, target_path)) = replacement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        else {
            return;
        };
        if expected_path != path {
            let mut pending = replacement
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *pending = Some((expected_path, displaced_path, target_path));
            return;
        }
        fs::rename(path, displaced_path).expect("original directory should be displaced");
        std::os::unix::fs::symlink(target_path, path)
            .expect("replacement symlink should be created");
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
fn maybe_replace_after_batch(path: &Path) {
    #[cfg(unix)]
    {
        let replacement = BATCH_REPLACEMENT.get_or_init(|| Mutex::new(None));
        let mut pending = replacement
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some((expected_path, displaced_path, target_path)) = pending.take() else {
            return;
        };
        if expected_path != path {
            *pending = Some((expected_path, displaced_path, target_path));
            return;
        }
        drop(pending);
        fs::rename(path, displaced_path).expect("original directory should be displaced");
        std::os::unix::fs::symlink(target_path, path)
            .expect("replacement symlink should be created");
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(all(test, unix))]
fn replace_after_next_validation(path: PathBuf, displaced: PathBuf, target: PathBuf) {
    *VALIDATION_REPLACEMENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path, displaced, target));
}

#[cfg(all(test, unix))]
pub(super) fn replace_after_next_batch(path: PathBuf, displaced: PathBuf, target: PathBuf) {
    *BATCH_REPLACEMENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((path, displaced, target));
}
fn report_directory_task_error(
    path: PathBuf,
    error: DirectoryTaskError,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
) {
    match error {
        DirectoryTaskError::Replaced(message) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanUnscanned {
                    path,
                    reason: UnscannedReason::Replacement(message),
                },
                cancelled,
            );
        }
        DirectoryTaskError::Io(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(path),
                    message: error.to_string(),
                },
                cancelled,
            );
        }
    }
}

fn validate_root_handle(
    root_directory: &File,
    expected: Option<&NativeIdentity>,
) -> io::Result<()> {
    let metadata = cap_fs::Metadata::from_file(root_directory)?;
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(io::Error::other("opened scan root is not a real directory"));
    }
    let Some(expected) = expected else {
        return Ok(());
    };
    let actual = identity_from_entry_metadata(&metadata)?
        .ok_or_else(|| io::Error::other("opened scan root identity is unavailable"))?;
    if !same_identity(expected, &actual) || actual.reparse_point {
        return Err(io::Error::other("opened scan root identity changed"));
    }
    Ok(())
}

#[cfg(windows)]
fn cap_metadata_is_reparse(metadata: &cap_fs::Metadata) -> bool {
    use cap_primitives::fs::_WindowsByHandle as _;

    metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn cap_metadata_is_reparse(metadata: &cap_fs::Metadata) -> bool {
    metadata.is_symlink()
}

fn open_scan_directory(root_directory: &File, root: &Path, path: &Path) -> io::Result<File> {
    let relative = task_relative_path(root, path)?;
    let mut directory = root_directory.try_clone()?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        directory = cap_fs::open_dir_nofollow(&directory, Path::new(name))?;
        let metadata = cap_fs::Metadata::from_file(&directory)?;
        if cap_metadata_is_reparse(&metadata) {
            return Err(io::Error::other(
                "scanner path component is a symbolic link or reparse point",
            ));
        }
    }
    Ok(directory)
}

fn task_relative_path(root: &Path, path: &Path) -> io::Result<PathBuf> {
    let relative = path.strip_prefix(root).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "scanner task path is outside the scan root",
        )
    })?;
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "scanner task path is not a safe relative path",
        ));
    }
    Ok(relative.to_path_buf())
}

#[cfg(windows)]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;

    metadata.file_type().is_symlink() || metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn path_has_reparse_component(path: &Path) -> bool {
    let mut current = PathBuf::new();
    path.components().any(|component| {
        current.push(component);
        fs::symlink_metadata(&current).is_ok_and(|metadata| metadata_is_reparse(&metadata))
    })
}

fn classify_directory_open_error(path: &Path, error: io::Error) -> DirectoryTaskError {
    let is_non_directory = fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_dir());
    if is_non_directory || path_has_reparse_component(path) {
        DirectoryTaskError::Replaced(
            "scanner directory task was replaced by a symbolic link or non-directory".to_string(),
        )
    } else {
        DirectoryTaskError::Io(error)
    }
}

fn same_identity(left: &NativeIdentity, right: &NativeIdentity) -> bool {
    left.file_id == right.file_id && left.reparse_point == right.reparse_point
}

fn open_frame(
    root_directory: &File,
    root: &Path,
    task: DirectoryTask,
) -> Result<ScanFrame, Box<(DirectoryTask, DirectoryTaskError)>> {
    let directory = match open_scan_directory(root_directory, root, &task.path) {
        Ok(directory) => directory,
        Err(error) => {
            return Err(Box::new((
                task.clone(),
                classify_directory_open_error(&task.path, error),
            )));
        }
    };
    let metadata = match cap_fs::Metadata::from_file(&directory) {
        Ok(metadata) => metadata,
        Err(error) => return Err(Box::new((task, DirectoryTaskError::Io(error)))),
    };
    if !metadata.is_dir() || metadata.is_symlink() {
        return Err(Box::new((
            task,
            DirectoryTaskError::Replaced(
                "scanner directory task was replaced by a symbolic link or non-directory"
                    .to_string(),
            ),
        )));
    }
    let actual = match identity_from_entry_metadata(&metadata) {
        Ok(Some(identity)) => identity,
        Ok(None) => {
            return Err(Box::new((
                task,
                DirectoryTaskError::Replaced(
                    "scanner directory task identity is unavailable".to_string(),
                ),
            )));
        }
        Err(error) => return Err(Box::new((task, DirectoryTaskError::Io(error)))),
    };
    if actual.reparse_point
        || task
            .identity
            .as_ref()
            .is_some_and(|expected| !same_identity(expected, &actual))
    {
        return Err(Box::new((
            task,
            DirectoryTaskError::Replaced(
                "scanner directory task identity changed before traversal".to_string(),
            ),
        )));
    }
    let entries = match cap_fs::read_base_dir(&directory) {
        Ok(entries) => entries,
        Err(error) => return Err(Box::new((task, DirectoryTaskError::Io(error)))),
    };
    Ok(ScanFrame {
        task,
        _directory: directory,
        identity: Some(actual),
        entries,
        batch: Vec::with_capacity(BATCH_SIZE),
        directories: Vec::with_capacity(BATCH_SIZE),
    })
}

#[allow(clippy::unnecessary_wraps)]
fn identity_from_entry_metadata(metadata: &cap_fs::Metadata) -> io::Result<Option<NativeIdentity>> {
    #[cfg(unix)]
    {
        use cap_primitives::fs::MetadataExt as _;
        Ok(Some(NativeIdentity {
            file_id: file_id::FileId::new_inode(metadata.dev(), metadata.ino()),
            link_count: Some(metadata.nlink()),
            reparse_point: metadata.is_symlink(),
        }))
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::_WindowsByHandle as _;
        let volume = metadata.volume_serial_number().ok_or_else(|| {
            io::Error::other("directory entry did not expose a volume serial number")
        })?;
        let index = metadata
            .file_index()
            .ok_or_else(|| io::Error::other("directory entry did not expose a file index"))?;
        Ok(Some(NativeIdentity {
            file_id: file_id::FileId::new_low_res(volume, index),
            link_count: metadata.number_of_links().map(u64::from),
            reparse_point: metadata.file_attributes() & 0x0000_0400 != 0,
        }))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        Ok(None)
    }
}

fn report_symbolic_link(path: PathBuf, sender: &Sender<WorkerEvent>, cancelled: &AtomicBool) {
    let _ = send_event(
        sender,
        WorkerEvent::ScanUnscanned {
            path,
            reason: UnscannedReason::SymbolicLink,
        },
        cancelled,
    );
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn process_entry(
    frame: &mut ScanFrame,
    entry: &cap_fs::DirEntry,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) {
    let name = entry.file_name();
    let path = frame.task.path.join(&name);
    if exclusions.is_internal(&path) {
        return;
    }
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(path),
                    message: error.to_string(),
                },
                cancelled,
            );
            return;
        }
    };
    let identity = match identity_for(&path, &metadata) {
        Ok(Some(identity)) => identity,
        Ok(None) => return,
        Err(error) => {
            let message = format!(
                "{}: {error}",
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_string_lossy()
            );
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(path),
                    message,
                },
                cancelled,
            );
            return;
        }
    };
    let is_dir = metadata.is_dir();
    if let Some(pattern) = exclusions.reason(&path, is_dir) {
        let _ = send_event(
            sender,
            WorkerEvent::ScanUnscanned {
                path,
                reason: UnscannedReason::Excluded(pattern),
            },
            cancelled,
        );
        return;
    }
    if metadata.file_type().is_symlink() || identity.reparse_point {
        report_symbolic_link(path, sender, cancelled);
        return;
    }
    if is_dir && !cross_filesystems {
        let reason = match root_filesystem {
            Some(root_filesystem) => filesystem_boundary_reason(&path, &metadata, root_filesystem),
            None => Some(UnscannedReason::Metadata(
                "root filesystem identity is unavailable".to_string(),
            )),
        };
        if let Some(reason) = reason {
            let _ = send_event(
                sender,
                WorkerEvent::ScanUnscanned { path, reason },
                cancelled,
            );
            return;
        }
    }
    let directory = is_dir.then(|| DirectoryTask {
        path: path.clone(),
        identity: Some(identity.clone()),
    });
    frame.batch.push(ScannedEntry {
        metadata,
        path,
        identity,
    });
    if let Some(directory) = directory {
        frame.directories.push(directory);
    }
}

fn flush_frame(
    frame: &mut ScanFrame,
    queue: &TaskQueue,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    failed: &AtomicBool,
) -> bool {
    let entries = std::mem::replace(&mut frame.batch, Vec::with_capacity(BATCH_SIZE));
    if !send_event(sender, WorkerEvent::ScanBatch { entries }, cancelled) {
        frame.directories.clear();
        return false;
    }
    // Let replacement-race fixtures mutate only after the batch is observable.
    #[cfg(test)]
    maybe_replace_after_batch(&frame.task.path);
    for task in std::mem::take(&mut frame.directories) {
        if let Err(error) = queue.schedule(task) {
            failed.store(true, Ordering::Release);
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: None,
                    message: format!("could not queue scanner directory: {error}"),
                },
                cancelled,
            );
            return false;
        }
    }
    true
}

fn validate_scan_root(path: &Path, expected: Option<&NativeIdentity>) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "could not inspect scan root {}: {error}",
            path.to_string_lossy()
        )
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("scan root was replaced by a symbolic link or non-directory".to_string());
    }
    let actual = identity_for(path, &metadata)
        .map_err(|error| format!("could not identify scan root: {error}"))?
        .ok_or_else(|| "scan root identity is unavailable".to_string())?;
    if actual.reparse_point {
        return Err("scan root was replaced by a reparse point".to_string());
    }
    if expected.is_some_and(|expected| {
        actual.file_id != expected.file_id || actual.reparse_point != expected.reparse_point
    }) {
        return Err("scan root identity changed before scanning".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scanner_reports_temporary_storage_exhaustion_without_completing_the_root() {
        use crossbeam_channel::unbounded;

        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..=TASK_QUEUE_PER_WORKER {
            fs::create_dir(root.path().join(format!("directory-{index}")))
                .expect("directory fixture should be created");
        }
        let (sender, events) = unbounded();
        let cancelled = AtomicBool::new(false);
        run(
            ScannerOptions {
                root: root.path().to_path_buf(),
                root_identity: None,
                threads: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                internal_paths: Vec::new(),
                temporary_storage: TemporaryStorage::with_limit_bytes(0),
            },
            &sender,
            &cancelled,
        );

        let mut saw_capacity_error = false;
        let mut completed_root = false;
        let mut finished = false;
        for event in events.try_iter() {
            match event {
                WorkerEvent::ScanFailed { message, .. } => {
                    saw_capacity_error |= message.contains("temporary storage capacity exhausted")
                        && message.contains("--temporary-storage-mib");
                }
                WorkerEvent::ScanDirectoryComplete { path, .. } => {
                    completed_root |= path == root.path();
                }
                WorkerEvent::ScanFinished { cancelled } => finished |= !cancelled,
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(
            saw_capacity_error,
            "temporary-storage exhaustion should be actionable"
        );
        assert!(
            finished,
            "scanner should report a terminal event after failure"
        );
        assert!(
            !completed_root,
            "a capacity failure must not report the partially queued root as complete"
        );
    }

    #[cfg(any(unix, windows))]
    fn replace_directory_with_link(path: &Path, displaced: &Path, target: &Path) {
        fs::rename(path, displaced).expect("original directory should be displaced");
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, path).expect("replacement symlink should be created");
        #[cfg(windows)]
        {
            let quote =
                |value: &Path| format!("'{}'", value.display().to_string().replace('\'', "''"));
            let command = format!(
                "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
                quote(path),
                quote(target)
            );
            let output = std::process::Command::new("pwsh")
                .args([
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-Command",
                    &command,
                ])
                .output()
                .expect("junction command should start");
            assert!(
                output.status.success(),
                "junction command failed with {}: stdout={:?} stderr={:?}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn scanner_marks_replaced_descendant_link_uncertain() {
        use crossbeam_channel::bounded;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        fs::write(outside.path().join("secret"), b"outside")
            .expect("outside fixture should be written");
        let descendant = root.path().join("descendant");
        let displaced = root.path().join("displaced-descendant");
        fs::create_dir(&descendant).expect("descendant should be created");
        fs::write(descendant.join("original"), b"inside")
            .expect("descendant fixture should be written");
        let metadata = fs::symlink_metadata(&descendant).expect("descendant metadata should exist");
        let identity = identity_for(&descendant, &metadata)
            .expect("descendant identity should be readable")
            .expect("descendant identity should be available");
        replace_directory_with_link(&descendant, &displaced, outside.path());

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(4);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_directory = cap_fs::open_ambient_dir(root.path(), ambient_authority())
            .expect("scan root handle should open");
        let exclusions = Exclusions::new(root.path(), Vec::new(), Vec::new())
            .expect("scanner exclusions should compile");
        assert!(scan_directory(
            DirectoryTask {
                path: descendant.clone(),
                identity: Some(identity),
            },
            &queue,
            &sender,
            &cancelled,
            &failed,
            &root_invalid,
            &root_directory,
            root.path(),
            None,
            &exclusions,
            None,
            true,
        ));

        match events
            .recv_timeout(Duration::from_secs(5))
            .expect("replaced descendant should be reported")
        {
            WorkerEvent::ScanUnscanned { path, reason } => {
                assert_eq!(path, descendant);
                assert!(matches!(reason, UnscannedReason::Replacement(_)));
            }
            _ => panic!("replaced descendant should not be traversed"),
        }
        assert!(
            events.try_recv().is_err(),
            "replacement must not be traversed"
        );
    }
    #[cfg(unix)]
    #[test]
    fn scanner_does_not_follow_replacement_after_identity_validation() {
        use crossbeam_channel::bounded;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let outside_secret = outside.path().join("secret");
        fs::write(&outside_secret, b"outside").expect("outside fixture should be written");
        let descendant = root.path().join("descendant");
        let displaced = root.path().join("displaced-descendant");
        fs::create_dir(&descendant).expect("descendant should be created");
        fs::write(descendant.join("original"), b"inside")
            .expect("descendant fixture should be written");
        let metadata = fs::symlink_metadata(&descendant).expect("descendant metadata should exist");
        let identity = identity_for(&descendant, &metadata)
            .expect("descendant identity should be readable")
            .expect("descendant identity should be available");
        replace_after_next_validation(descendant.clone(), displaced, outside.path().to_path_buf());

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(8);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_directory = cap_fs::open_ambient_dir(root.path(), ambient_authority())
            .expect("scan root handle should open");
        let exclusions = Exclusions::new(root.path(), Vec::new(), Vec::new())
            .expect("scanner exclusions should compile");
        assert!(scan_directory(
            DirectoryTask {
                path: descendant.clone(),
                identity: Some(identity),
            },
            &queue,
            &sender,
            &cancelled,
            &failed,
            &root_invalid,
            &root_directory,
            root.path(),
            None,
            &exclusions,
            None,
            true,
        ));

        let mut saw_replacement = false;
        while let Ok(event) = events.recv_timeout(Duration::from_secs(1)) {
            match event {
                WorkerEvent::ScanUnscanned { path, reason } => {
                    assert_eq!(path, descendant);
                    assert!(matches!(reason, UnscannedReason::Replacement(_)));
                    saw_replacement = true;
                    break;
                }
                WorkerEvent::ScanBatch { entries } => {
                    assert!(
                        !entries
                            .iter()
                            .any(|entry| entry.path == descendant.join("secret")),
                        "scanner must not enumerate the replacement target"
                    );
                }
                WorkerEvent::ScanDirectoryComplete { .. } => {
                    panic!("replaced directory must not be reported complete")
                }
                WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(
            saw_replacement,
            "replacement should be reported as unscanned"
        );
        assert!(
            outside_secret.exists(),
            "outside target should remain untouched"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn scanner_rejects_replaced_ancestor_before_traversal() {
        use crossbeam_channel::bounded;
        use std::sync::atomic::AtomicBool;
        use std::time::Duration;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        let ancestor = root.path().join("ancestor");
        let descendant = ancestor.join("descendant");
        fs::create_dir_all(&descendant).expect("descendant should be created");
        fs::write(descendant.join("original"), b"inside")
            .expect("descendant fixture should be written");
        let metadata = fs::symlink_metadata(&descendant).expect("descendant metadata should exist");
        let identity = identity_for(&descendant, &metadata)
            .expect("descendant identity should be readable")
            .expect("descendant identity should be available");

        let moved = outside.path().join("moved-ancestor");
        replace_directory_with_link(&ancestor, &moved, &moved);
        let outside_only = moved.join("descendant").join("outside-only");
        let escaped_path = descendant.join("outside-only");
        fs::write(&outside_only, b"outside").expect("outside fixture should be written");

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(8);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_directory = cap_fs::open_ambient_dir(root.path(), ambient_authority())
            .expect("scan root handle should open");
        let exclusions = Exclusions::new(root.path(), Vec::new(), Vec::new())
            .expect("scanner exclusions should compile");
        assert!(scan_directory(
            DirectoryTask {
                path: descendant.clone(),
                identity: Some(identity),
            },
            &queue,
            &sender,
            &cancelled,
            &failed,
            &root_invalid,
            &root_directory,
            root.path(),
            None,
            &exclusions,
            None,
            true,
        ));

        let mut saw_replacement = false;
        while let Ok(event) = events.recv_timeout(Duration::from_secs(1)) {
            match event {
                WorkerEvent::ScanUnscanned { path, reason } => {
                    assert_eq!(path, descendant);
                    assert!(matches!(reason, UnscannedReason::Replacement(_)));
                    saw_replacement = true;
                    break;
                }
                WorkerEvent::ScanBatch { entries } => {
                    assert!(
                        !entries.iter().any(|entry| entry.path == escaped_path),
                        "scanner must not enumerate the moved ancestor"
                    );
                }
                WorkerEvent::ScanDirectoryComplete { .. } => {
                    panic!("replaced ancestor must not be reported complete")
                }
                WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(
            saw_replacement,
            "replaced ancestor should be reported as unscanned"
        );
        assert!(
            outside_only.exists(),
            "outside target should remain untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn scanner_validated_completion_trusts_provided_identity() {
        use crate::model::{MIN_PROCESS_MIB, NodeKind, NodeState};
        use crate::state::files::FileTree;
        use crossbeam_channel::bounded;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        fs::write(outside.path().join("secret"), b"outside")
            .expect("outside fixture should be written");
        let descendant = root.path().join("descendant");
        let displaced = root.path().join("displaced-descendant");
        fs::create_dir(&descendant).expect("descendant should be created");
        fs::write(descendant.join("original"), b"inside")
            .expect("descendant fixture should be written");
        let metadata = fs::symlink_metadata(&descendant).expect("descendant metadata should exist");
        let identity = identity_for(&descendant, &metadata)
            .expect("descendant identity should be readable")
            .expect("descendant identity should be available");

        let mut tree = FileTree::new(root.path().to_path_buf(), true, MIN_PROCESS_MIB)
            .expect("file tree should be created");
        tree.add_entry(&metadata, &descendant, identity.clone())
            .expect("descendant should be represented");

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(8);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_directory = cap_fs::open_ambient_dir(root.path(), ambient_authority())
            .expect("scan root handle should open");
        let exclusions = Exclusions::new(root.path(), Vec::new(), Vec::new())
            .expect("scanner exclusions should compile");
        assert!(scan_directory(
            DirectoryTask {
                path: descendant.clone(),
                identity: Some(identity.clone()),
            },
            &queue,
            &sender,
            &cancelled,
            &failed,
            &root_invalid,
            &root_directory,
            root.path(),
            None,
            &exclusions,
            None,
            true,
        ));

        replace_directory_with_link(&descendant, &displaced, outside.path());

        let mut saw_completion = false;
        for event in events.try_iter() {
            match event {
                WorkerEvent::ScanBatch { entries } => {
                    for entry in entries {
                        tree.add_entry(&entry.metadata, &entry.path, entry.identity)
                            .expect("scanned entry should be represented");
                    }
                }
                WorkerEvent::ScanDirectoryComplete { path, identity } => {
                    assert_eq!(path, descendant);
                    assert!(identity.is_some());
                    tree.complete_directory(&path, identity.as_ref())
                        .expect("completion should be represented");
                    saw_completion = true;
                }
                WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(saw_completion, "scan should emit a completion event");
        let node = tree
            .nodes()
            .find(|node| tree.path_for_id(node.id).as_deref() == Some(descendant.as_path()))
            .expect("directory should remain represented");
        assert_eq!(node.kind, NodeKind::Directory);
        assert_eq!(node.state, NodeState::Complete);
        assert!(outside.path().join("secret").exists());
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum FilesystemKey {
    #[cfg(unix)]
    Unix(u64),
    #[cfg(windows)]
    Windows(OsString),
    #[cfg(not(any(unix, windows)))]
    Other,
}

#[cfg(unix)]
fn filesystem_key(path: &Path) -> std::io::Result<FilesystemKey> {
    let metadata = fs::metadata(path)?;
    Ok(filesystem_key_with_metadata(path, &metadata))
}

#[cfg(windows)]
fn filesystem_key(path: &Path) -> std::io::Result<FilesystemKey> {
    let metadata = fs::metadata(path)?;
    filesystem_key_with_metadata(path, &metadata)
}

#[cfg(not(any(unix, windows)))]
fn filesystem_key(path: &Path) -> std::io::Result<FilesystemKey> {
    let metadata = fs::metadata(path)?;
    Ok(filesystem_key_with_metadata(path, &metadata))
}

#[cfg(unix)]
fn filesystem_key_with_metadata(_path: &Path, metadata: &fs::Metadata) -> FilesystemKey {
    use std::os::unix::fs::MetadataExt as _;

    FilesystemKey::Unix(metadata.dev())
}

#[cfg(windows)]
fn filesystem_key_with_metadata(
    path: &Path,
    _metadata: &fs::Metadata,
) -> std::io::Result<FilesystemKey> {
    use std::path::Component;

    let prefix = path
        .components()
        .find_map(|component| match component {
            Component::Prefix(prefix) => Some(prefix.as_os_str().to_os_string()),
            _ => None,
        })
        .ok_or_else(|| std::io::Error::other("path has no Windows volume prefix"))?;
    Ok(FilesystemKey::Windows(prefix))
}

#[cfg(not(any(unix, windows)))]
fn filesystem_key_with_metadata(_path: &Path, _metadata: &fs::Metadata) -> FilesystemKey {
    FilesystemKey::Other
}

#[cfg(unix)]
fn filesystem_boundary_reason(
    path: &Path,
    metadata: &fs::Metadata,
    root_filesystem: &FilesystemKey,
) -> Option<UnscannedReason> {
    let current = filesystem_key_with_metadata(path, metadata);
    if &current == root_filesystem {
        None
    } else {
        Some(UnscannedReason::FilesystemBoundary)
    }
}

#[cfg(windows)]
fn filesystem_boundary_reason(
    path: &Path,
    metadata: &fs::Metadata,
    root_filesystem: &FilesystemKey,
) -> Option<UnscannedReason> {
    match filesystem_key_with_metadata(path, metadata) {
        Ok(current) if &current != root_filesystem => Some(UnscannedReason::FilesystemBoundary),
        Ok(_) => None,
        Err(error) => Some(UnscannedReason::Metadata(format!(
            "could not identify filesystem: {error}"
        ))),
    }
}

#[cfg(not(any(unix, windows)))]
fn filesystem_boundary_reason(
    path: &Path,
    metadata: &fs::Metadata,
    root_filesystem: &FilesystemKey,
) -> Option<UnscannedReason> {
    let current = filesystem_key_with_metadata(path, metadata);
    if &current == root_filesystem {
        None
    } else {
        Some(UnscannedReason::FilesystemBoundary)
    }
}
