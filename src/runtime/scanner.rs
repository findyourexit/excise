use std::collections::VecDeque;
#[cfg(windows)]
use std::ffi::OsString;
use std::fs;
#[cfg(not(windows))]
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

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
        let payload = serde_json::to_vec(&encoded).map_err(io::Error::other)?;
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
        let encoded: EncodedNativePath =
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
        Ok(Some(DirectoryTask { path }))
    }
}

impl TaskQueue {
    fn new(root: PathBuf, capacity: usize) -> io::Result<(Self, Option<PathBuf>)> {
        let (spill, spill_path) = TaskSpill::new()?;
        Ok((
            Self {
                state: Mutex::new(QueueState {
                    tasks: VecDeque::from([DirectoryTask { path: root }]),
                    spill,
                    pending: 1,
                }),
                ready: Condvar::new(),
                capacity,
            },
            spill_path,
        ))
    }

    fn take(&self, cancelled: &AtomicBool) -> io::Result<Option<DirectoryTask>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if cancelled.load(Ordering::Acquire) || state.pending == 0 {
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
    thread::scope(|scope| {
        for index in 0..threads {
            let worker_queue = &queue;
            let worker_sender = sender;
            let worker_cancelled = cancelled;
            let worker_exclusions = &exclusions;
            let worker_root_filesystem = root_filesystem.as_ref();
            if let Err(error) = thread::Builder::new()
                .name(format!("excise-scan-{index}"))
                .spawn_scoped(scope, move || {
                    scan_worker(
                        worker_queue,
                        worker_sender,
                        worker_cancelled,
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
    entries: fs::ReadDir,
    batch: Vec<ScannedEntry>,
    directories: Vec<DirectoryTask>,
}

fn scan_worker(
    queue: &TaskQueue,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) {
    loop {
        let task = match queue.take(cancelled) {
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
            exclusions,
            root_filesystem,
            cross_filesystems,
        );
        queue.complete();
        if !completed {
            cancelled.store(true, Ordering::Release);
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
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) -> bool {
    if cancelled.load(Ordering::Acquire) {
        return false;
    }
    let mut frame = match open_frame(task) {
        Ok(frame) => frame,
        Err((task, error)) => {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFailed {
                    path: Some(task.path),
                    message: error.to_string(),
                },
                cancelled,
            );
            return true;
        }
    };
    loop {
        if cancelled.load(Ordering::Acquire) {
            return false;
        }
        if frame.batch.len() == BATCH_SIZE && !flush_frame(&mut frame, queue, sender, cancelled) {
            return false;
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
    if !frame.batch.is_empty() && !flush_frame(&mut frame, queue, sender, cancelled) {
        return false;
    }
    send_event(
        sender,
        WorkerEvent::ScanDirectoryComplete {
            path: frame.task.path,
        },
        cancelled,
    )
}

fn open_frame(task: DirectoryTask) -> Result<ScanFrame, (DirectoryTask, std::io::Error)> {
    match fs::read_dir(&task.path) {
        Ok(entries) => Ok(ScanFrame {
            task,
            entries,
            batch: Vec::with_capacity(BATCH_SIZE),
            directories: Vec::with_capacity(BATCH_SIZE),
        }),
        Err(error) => Err((task, error)),
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn process_entry(
    frame: &mut ScanFrame,
    entry: &fs::DirEntry,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
    exclusions: &Exclusions,
    root_filesystem: Option<&FilesystemKey>,
    cross_filesystems: bool,
) {
    let path = entry.path();
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
    if metadata.file_type().is_symlink() {
        let _ = send_event(
            sender,
            WorkerEvent::ScanUnscanned {
                path,
                reason: UnscannedReason::SymbolicLink,
            },
            cancelled,
        );
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
    match identity_for(&path, &metadata) {
        Ok(Some(identity)) => {
            let directory = is_dir.then(|| DirectoryTask { path: path.clone() });
            frame.batch.push(ScannedEntry {
                metadata,
                path,
                identity,
            });
            if let Some(directory) = directory {
                frame.directories.push(directory);
            }
        }
        Ok(None) => {}
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
        }
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
