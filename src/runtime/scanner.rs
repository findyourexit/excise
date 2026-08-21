use std::collections::VecDeque;
#[cfg(windows)]
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use cap_primitives::ambient_authority;
use cap_primitives::fs::{self as cap_fs, FollowSymlinks};
use crossbeam_channel::Sender;
use ignore::gitignore::{Gitignore, GitignoreBuilder};

use super::worker::{ScannedEntry, WorkerEvent, send_event};
use crate::model::UnscannedReason;
use crate::native_path::{EncodedNativePath, NativeIdentity, NativePath, identity_for};

const BATCH_SIZE: usize = 128;
const TASK_QUEUE_PER_WORKER: usize = 8;
const MAX_SPILLED_TASK_BYTES: usize = 1024 * 1024;
const SPILL_LENGTH_BYTES: u64 = 8;
#[cfg(windows)]
type TaskSpillFile = tempfile::NamedTempFile;
#[cfg(not(windows))]
type TaskSpillFile = File;

#[derive(Clone, Debug)]
pub struct ScannerOptions {
    pub root: PathBuf,
    pub root_identity: Option<NativeIdentity>,
    pub threads: usize,
    pub cross_filesystems: bool,
    pub exclusions: Vec<String>,
    pub internal_paths: Vec<PathBuf>,
}

#[derive(Clone)]
struct DirectoryTask {
    path: PathBuf,
    identity: Option<NativeIdentity>,
}

struct TaskQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    capacity: usize,
}

struct QueueState {
    tasks: VecDeque<DirectoryTask>,
    spill: TaskSpill,
    pending: usize,
}

struct TaskSpill {
    file: TaskSpillFile,
    next_read: u64,
    next_write: u64,
    pending: usize,
}

impl TaskSpill {
    fn new() -> io::Result<(Self, Option<PathBuf>)> {
        #[cfg(windows)]
        let (file, spill_path) = {
            let file = tempfile::NamedTempFile::new()?;
            crate::os::windows::restrict_private_path(file.path(), false)?;
            crate::os::windows::verify_private_path(file.path(), false)?;
            let spill_path = Some(file.path().to_path_buf());
            (file, spill_path)
        };
        #[cfg(not(windows))]
        let (file, spill_path) = (tempfile::tempfile()?, None);

        Ok((
            Self {
                file,
                next_read: 0,
                next_write: 0,
                pending: 0,
            },
            spill_path,
        ))
    }

    fn push(&mut self, task: DirectoryTask) -> io::Result<()> {
        let encoded = NativePath::new(task.path).encode();
        let payload = serde_json::to_vec(&(encoded, task.identity)).map_err(io::Error::other)?;
        if payload.len() > MAX_SPILLED_TASK_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task path exceeds the spill record limit",
            ));
        }
        let payload_len = u64::try_from(payload.len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task path length does not fit in a spill record",
            )
        })?;
        let next_write = self
            .next_write
            .checked_add(SPILL_LENGTH_BYTES)
            .and_then(|offset| offset.checked_add(payload_len))
            .ok_or_else(|| io::Error::other("scanner task spill offset overflow"))?;
        let pending = self
            .pending
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scanner task spill count overflow"))?;

        self.file.seek(SeekFrom::Start(self.next_write))?;
        self.file.write_all(&payload_len.to_le_bytes())?;
        self.file.write_all(&payload)?;
        self.next_write = next_write;
        self.pending = pending;
        Ok(())
    }

    fn take(&mut self) -> io::Result<Option<DirectoryTask>> {
        if self.pending == 0 {
            return Ok(None);
        }

        self.file.seek(SeekFrom::Start(self.next_read))?;
        let mut length = [0_u8; std::mem::size_of::<u64>()];
        self.file.read_exact(&mut length)?;
        let payload_len = usize::try_from(u64::from_le_bytes(length)).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task spill record length does not fit in memory",
            )
        })?;
        if payload_len > MAX_SPILLED_TASK_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "scanner task spill record exceeds the configured limit",
            ));
        }
        let mut payload = vec![0_u8; payload_len];
        self.file.read_exact(&mut payload)?;
        let (encoded, identity): (EncodedNativePath, Option<NativeIdentity>) =
            serde_json::from_slice(&payload).map_err(io::Error::other)?;
        let path = NativePath::decode(&encoded)
            .map_err(io::Error::other)?
            .as_path()
            .to_path_buf();
        let record_len = u64::try_from(payload_len)
            .map_err(|_| io::Error::other("scanner task spill record length overflow"))?;
        self.next_read = self
            .next_read
            .checked_add(SPILL_LENGTH_BYTES)
            .and_then(|offset| offset.checked_add(record_len))
            .ok_or_else(|| io::Error::other("scanner task spill offset overflow"))?;
        self.pending = self
            .pending
            .checked_sub(1)
            .ok_or_else(|| io::Error::other("scanner task spill count underflow"))?;
        if self.pending == 0 {
            self.next_read = 0;
            self.next_write = 0;
            #[cfg(windows)]
            let _ = self.file.as_file().set_len(0);
            #[cfg(not(windows))]
            let _ = self.file.set_len(0);
        }
        Ok(Some(DirectoryTask { path, identity }))
    }
}

impl TaskQueue {
    fn new(root: PathBuf, capacity: usize) -> io::Result<(Self, Option<PathBuf>)> {
        let (spill, spill_path) = TaskSpill::new()?;
        Ok((
            Self {
                state: Mutex::new(QueueState {
                    tasks: VecDeque::from([DirectoryTask {
                        path: root,
                        identity: None,
                    }]),
                    spill,
                    pending: 1,
                }),
                ready: Condvar::new(),
                capacity,
            },
            spill_path,
        ))
    }

    fn take(
        &self,
        cancelled: &AtomicBool,
        root_invalid: &AtomicBool,
    ) -> io::Result<Option<DirectoryTask>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if cancelled.load(Ordering::Acquire)
                || root_invalid.load(Ordering::Acquire)
                || state.pending == 0
            {
                return Ok(None);
            }
            if let Some(task) = state.tasks.pop_front() {
                return Ok(Some(task));
            }
            if let Some(task) = state.spill.take()? {
                return Ok(Some(task));
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn schedule(&self, task: DirectoryTask) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let pending = state
            .pending
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scanner task count overflow"))?;
        if state.tasks.len() < self.capacity {
            state.tasks.push_back(task);
        } else {
            state.spill.push(task)?;
        }
        state.pending = pending;
        self.ready.notify_one();
        Ok(())
    }

    fn complete(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = state.pending.saturating_sub(1);
        self.ready.notify_all();
    }

    fn cancel(&self) {
        self.ready.notify_all();
    }
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
    let (queue, task_spill_path) = match TaskQueue::new(
        root.clone(),
        threads.saturating_mul(TASK_QUEUE_PER_WORKER).max(1),
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
    thread::scope(|scope| {
        for index in 0..threads {
            let worker_queue = &queue;
            let worker_sender = sender;
            let worker_cancelled = cancelled;
            let worker_root_invalid = &root_invalid;
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
                        worker_root_invalid,
                        worker_root,
                        worker_root_identity,
                        worker_exclusions,
                        worker_root_filesystem,
                        cross_filesystems,
                    );
                })
            {
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
            cancelled: cancelled.load(Ordering::Acquire),
        },
        &completion_delivery,
    );
}

struct ScanFrame {
    task: DirectoryTask,
    _directory: File,
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
    root_invalid: &AtomicBool,
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) {
    loop {
        let task = match queue.take(cancelled, root_invalid) {
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
                cancelled.store(true, Ordering::Release);
                queue.cancel();
                return;
            }
        };
        let completed = scan_directory(
            task,
            queue,
            sender,
            cancelled,
            root_invalid,
            root,
            root_identity,
            exclusions,
            root_filesystem,
            cross_filesystems,
        );
        queue.complete();
        if !completed {
            if !root_invalid.load(Ordering::Acquire) {
                cancelled.store(true, Ordering::Release);
            }
            queue.cancel();
            return;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_directory(
    task: DirectoryTask,
    queue: &TaskQueue,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    root_invalid: &AtomicBool,
    root: &Path,
    root_identity: Option<&NativeIdentity>,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) -> bool {
    if cancelled.load(Ordering::Acquire) || root_invalid.load(Ordering::Acquire) {
        return false;
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return false;
    }
    let mut frame = match open_frame(task) {
        Ok(frame) => frame,
        Err(error) => {
            let (task, error) = *error;
            report_directory_task_error(task.path, error, sender, cancelled);
            return true;
        }
    };
    if let Err(error) = validate_directory_task(&frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
        return true;
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return false;
    }
    loop {
        if cancelled.load(Ordering::Acquire)
            || !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid)
        {
            return false;
        }
        if frame.batch.len() == BATCH_SIZE {
            if !flush_frame(&mut frame, queue, sender, cancelled) {
                return false;
            }
            if let Err(error) = validate_directory_task(&frame.task) {
                report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
                return true;
            }
        }
        match frame.entries.next() {
            Some(Ok(entry)) => {
                if !validate_root_for_traversal(
                    root,
                    root_identity,
                    sender,
                    cancelled,
                    root_invalid,
                ) {
                    return false;
                }
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
    if let Err(error) = validate_directory_task(&frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
        return true;
    }
    if !frame.batch.is_empty() && !flush_frame(&mut frame, queue, sender, cancelled) {
        return false;
    }
    if let Err(error) = validate_directory_task(&frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
        return true;
    }
    if !validate_root_for_traversal(root, root_identity, sender, cancelled, root_invalid) {
        return false;
    }
    if let Err(error) = validate_directory_task(&frame.task) {
        report_directory_task_error(frame.task.path.clone(), error, sender, cancelled);
        return true;
    }
    send_event(
        sender,
        WorkerEvent::ScanDirectoryComplete {
            path: frame.task.path,
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

fn validate_directory_task(task: &DirectoryTask) -> Result<(), DirectoryTaskError> {
    let Some(expected) = task.identity.as_ref() else {
        return Ok(());
    };
    let metadata = fs::symlink_metadata(&task.path).map_err(DirectoryTaskError::Io)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(DirectoryTaskError::Replaced(
            "scanner directory task was replaced by a symbolic link or non-directory".to_string(),
        ));
    }
    let actual = identity_for(&task.path, &metadata)
        .map_err(DirectoryTaskError::Io)?
        .ok_or_else(|| {
            DirectoryTaskError::Replaced(
                "scanner directory task identity is unavailable".to_string(),
            )
        })?;
    if actual.file_id != expected.file_id
        || actual.reparse_point != expected.reparse_point
        || actual.reparse_point
    {
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

#[cfg(all(test, unix))]
fn replace_after_next_validation(path: PathBuf, displaced: PathBuf, target: PathBuf) {
    *VALIDATION_REPLACEMENT
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
                    reason: UnscannedReason::Metadata(message),
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

fn open_scan_directory(path: &Path) -> io::Result<File> {
    let mut options = cap_fs::OpenOptions::new();
    options.read(true)._cap_fs_ext_follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_primitives::fs::OpenOptionsExt as _;
        let flags = rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::NOFOLLOW;
        options.custom_flags(i32::try_from(flags.bits()).unwrap_or(i32::MAX));
    }
    #[cfg(windows)]
    {
        use cap_primitives::fs::OpenOptionsExt as _;
        const FILE_LIST_DIRECTORY: u32 = 0x0000_0001;
        const FILE_READ_ATTRIBUTES: u32 = 0x0000_0080;
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        const FILE_SHARE_WRITE: u32 = 0x0000_0002;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options
            .access_mode(FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | SYNCHRONIZE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
    }
    cap_fs::open_ambient(path, &options, ambient_authority())
}

fn classify_directory_open_error(path: &Path, error: io::Error) -> DirectoryTaskError {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_dir() || metadata.file_type().is_symlink() => {
            DirectoryTaskError::Replaced(
                "scanner directory task was replaced by a symbolic link or non-directory"
                    .to_string(),
            )
        }
        _ => DirectoryTaskError::Io(error),
    }
}

fn same_identity(left: &NativeIdentity, right: &NativeIdentity) -> bool {
    left.file_id == right.file_id && left.reparse_point == right.reparse_point
}

fn open_frame(task: DirectoryTask) -> Result<ScanFrame, Box<(DirectoryTask, DirectoryTaskError)>> {
    let directory = match open_scan_directory(&task.path) {
        Ok(directory) => directory,
        Err(error) => {
            return Err(Box::new((
                task.clone(),
                classify_directory_open_error(&task.path, error),
            )));
        }
    };
    let metadata = match directory.metadata() {
        Ok(metadata) => metadata,
        Err(error) => return Err(Box::new((task, DirectoryTaskError::Io(error)))),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(Box::new((
            task,
            DirectoryTaskError::Replaced(
                "scanner directory task was replaced by a symbolic link or non-directory"
                    .to_string(),
            ),
        )));
    }
    let actual = match identity_for(&task.path, &metadata) {
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
        entries,
        batch: Vec::with_capacity(BATCH_SIZE),
        directories: Vec::with_capacity(BATCH_SIZE),
    })
}

fn entry_metadata(entry: &cap_fs::DirEntry) -> io::Result<cap_fs::Metadata> {
    #[cfg(windows)]
    {
        use cap_primitives::fs::_WindowsDirEntryExt as _;
        entry.full_metadata()
    }
    #[cfg(not(windows))]
    {
        entry.metadata()
    }
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
    let entry_metadata = match entry_metadata(entry) {
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
    let entry_identity = match identity_from_entry_metadata(&entry_metadata) {
        Ok(identity) => identity,
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
    if entry_identity
        .as_ref()
        .is_some_and(|expected| !same_identity(expected, &identity))
    {
        let _ = send_event(
            sender,
            WorkerEvent::ScanUnscanned {
                path,
                reason: UnscannedReason::Metadata(
                    "scanner entry identity changed before metadata collection".to_string(),
                ),
            },
            cancelled,
        );
        return;
    }
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
        identity: Some(entry_identity.unwrap_or_else(|| identity.clone())),
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
) -> bool {
    let entries = std::mem::replace(&mut frame.batch, Vec::with_capacity(BATCH_SIZE));
    if !send_event(sender, WorkerEvent::ScanBatch { entries }, cancelled) {
        frame.directories.clear();
        return false;
    }
    for task in std::mem::take(&mut frame.directories) {
        if let Err(error) = queue.schedule(task) {
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
    fn task_queue_spills_overflow_without_growing_the_resident_queue() {
        let (queue, task_spill_path) = TaskQueue::new(PathBuf::from("/scan-root"), 1)
            .expect("scanner task spill should be available");
        #[cfg(windows)]
        {
            let task_spill_path = task_spill_path.expect("Windows task spill should be named");
            assert!(task_spill_path.is_absolute());
            crate::os::windows::verify_private_path(&task_spill_path, false)
                .expect("Windows task spill should have a private DACL");
            let exclusions = Exclusions::new(
                Path::new("/scan-root"),
                Vec::new(),
                vec![task_spill_path.clone()],
            )
            .expect("scanner task spill should be an exact internal exclusion");
            assert_eq!(
                exclusions.reason(&task_spill_path, false),
                Some("Excise session state".to_string())
            );
        }
        #[cfg(not(windows))]
        assert!(task_spill_path.is_none());
        for index in 0..BATCH_SIZE {
            queue
                .schedule(DirectoryTask {
                    path: PathBuf::from(format!("/scan-root/entry-{index}")),
                    identity: None,
                })
                .expect("scanner task should spill when the resident queue is full");
        }

        let state = queue
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.spill.pending, BATCH_SIZE);
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

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1)
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(4);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
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
            &root_invalid,
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
                assert!(matches!(reason, UnscannedReason::Metadata(_)));
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

        let (queue, _) = TaskQueue::new(root.path().to_path_buf(), 1)
            .expect("scanner task queue should be available");
        let (sender, events) = bounded(8);
        let cancelled = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
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
            &root_invalid,
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
                    assert!(matches!(reason, UnscannedReason::Metadata(_)));
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
