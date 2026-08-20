use std::fs::{self, Metadata};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, bounded};
use jwalk::Parallelism::{RayonNewPool, Serial};
use jwalk::WalkDir;

use crate::error::AppError;
use crate::native_path::{NativeIdentity, identity_for, safe_display_path};
use crate::state::FileToDelete;

const CHANNEL_RETRY: Duration = Duration::from_millis(25);

#[derive(Clone)]
pub struct DeletionRequest {
    pub target: FileToDelete,
    pub expected_identity: NativeIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeletionFailure {
    IdentityChanged,
    SymbolicLink,
    Io(String),
}

pub enum WorkerEvent {
    ScanEntry {
        metadata: Metadata,
        path: PathBuf,
        identity: NativeIdentity,
    },
    ScanSkippedLink {
        path: PathBuf,
    },
    ScanFailed {
        path: Option<PathBuf>,
        message: String,
    },
    ScanFinished {
        cancelled: bool,
    },
    DeletionFinished {
        request: DeletionRequest,
        result: Result<(), DeletionFailure>,
    },
}

enum WorkerCommand {
    Delete(DeletionRequest),
}

pub struct WorkerPool {
    events: Receiver<WorkerEvent>,
    commands: Sender<WorkerCommand>,
    cancelled: Arc<AtomicBool>,
    handles: Vec<thread::JoinHandle<()>>,
}

impl WorkerPool {
    pub fn start(
        root: PathBuf,
        scan_threads: usize,
        event_capacity: usize,
    ) -> Result<Self, AppError> {
        let (event_sender, events) = bounded(event_capacity);
        let (commands, command_receiver) = bounded(1);
        let cancelled = Arc::new(AtomicBool::new(false));

        let scanner_cancelled = cancelled.clone();
        let scanner_events = event_sender.clone();
        let scanner = thread::Builder::new()
            .name("excise-scanner".to_string())
            .spawn(move || scan(&root, scan_threads, &scanner_events, &scanner_cancelled))
            .map_err(|error| AppError::io("could not spawn scanner worker", error))?;

        let deletion_cancelled = cancelled.clone();
        let deletion = thread::Builder::new()
            .name("excise-deletion".to_string())
            .spawn(move || deletion_worker(&command_receiver, &event_sender, &deletion_cancelled))
            .map_err(|error| AppError::io("could not spawn deletion worker", error))?;

        Ok(Self {
            events,
            commands,
            cancelled,
            handles: vec![scanner, deletion],
        })
    }

    #[must_use]
    pub const fn events(&self) -> &Receiver<WorkerEvent> {
        &self.events
    }

    pub fn request_deletion(&self, request: DeletionRequest) -> Result<(), AppError> {
        self.commands
            .send(WorkerCommand::Delete(request))
            .map_err(|_| AppError::Worker("deletion worker disconnected".to_string()))
    }

    pub fn shutdown(self) -> Result<(), AppError> {
        self.cancelled.store(true, Ordering::Release);
        drop(self.events);
        drop(self.commands);
        for handle in self.handles {
            handle
                .join()
                .map_err(|_| AppError::Worker("worker thread panicked".to_string()))?;
        }
        Ok(())
    }
}

pub fn prepare_deletion(target: FileToDelete) -> Result<DeletionRequest, DeletionFailure> {
    let path = target.full_path();
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        DeletionFailure::Io(format!("{}: {error}", safe_display_path(&path).text))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DeletionFailure::SymbolicLink);
    }
    let expected_identity = identity_for(&path, &metadata)
        .map_err(|error| DeletionFailure::Io(error.to_string()))?
        .ok_or(DeletionFailure::SymbolicLink)?;
    Ok(DeletionRequest {
        target,
        expected_identity,
    })
}

fn scan(
    root: &std::path::Path,
    scan_threads: usize,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
) {
    let parallelism = if scan_threads == 1 {
        Serial
    } else {
        RayonNewPool(scan_threads)
    };
    for entry in WalkDir::new(root)
        .parallelism(parallelism)
        .skip_hidden(false)
        .follow_links(false)
    {
        if cancelled.load(Ordering::Acquire) {
            let _ = send_event(
                sender,
                WorkerEvent::ScanFinished { cancelled: true },
                cancelled,
            );
            return;
        }

        let event = match entry {
            Ok(entry) => {
                let path = entry.path();
                match fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        WorkerEvent::ScanSkippedLink { path }
                    }
                    Ok(metadata) => match identity_for(&path, &metadata) {
                        Ok(Some(identity)) => WorkerEvent::ScanEntry {
                            metadata,
                            path,
                            identity,
                        },
                        Ok(None) => WorkerEvent::ScanSkippedLink { path },
                        Err(error) => WorkerEvent::ScanFailed {
                            path: Some(path),
                            message: error.to_string(),
                        },
                    },
                    Err(error) => WorkerEvent::ScanFailed {
                        path: Some(path),
                        message: error.to_string(),
                    },
                }
            }
            Err(error) => WorkerEvent::ScanFailed {
                path: error.path().map(PathBuf::from),
                message: error.to_string(),
            },
        };
        if !send_event(sender, event, cancelled) {
            return;
        }
    }
    let _ = send_event(
        sender,
        WorkerEvent::ScanFinished { cancelled: false },
        cancelled,
    );
}

fn deletion_worker(
    commands: &Receiver<WorkerCommand>,
    sender: &Sender<WorkerEvent>,
    cancelled: &AtomicBool,
) {
    loop {
        if cancelled.load(Ordering::Acquire) {
            return;
        }
        let command = match commands.recv_timeout(CHANNEL_RETRY) {
            Ok(command) => command,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let WorkerCommand::Delete(request) = command;
        let result = execute_deletion(&request);
        if !send_event(
            sender,
            WorkerEvent::DeletionFinished { request, result },
            cancelled,
        ) {
            return;
        }
    }
}

fn execute_deletion(request: &DeletionRequest) -> Result<(), DeletionFailure> {
    let path = request.target.full_path();
    let metadata = fs::symlink_metadata(&path).map_err(|error| {
        DeletionFailure::Io(format!("{}: {error}", safe_display_path(&path).text))
    })?;
    if metadata.file_type().is_symlink() {
        return Err(DeletionFailure::SymbolicLink);
    }
    let actual = identity_for(&path, &metadata)
        .map_err(|error| DeletionFailure::Io(error.to_string()))?
        .ok_or(DeletionFailure::SymbolicLink)?;
    if actual != request.expected_identity {
        return Err(DeletionFailure::IdentityChanged);
    }

    if metadata.is_dir() {
        fs::remove_dir_all(&path)
    } else {
        fs::remove_file(&path)
    }
    .map_err(|error| DeletionFailure::Io(error.to_string()))
}

fn send_event(
    sender: &Sender<WorkerEvent>,
    mut event: WorkerEvent,
    cancelled: &AtomicBool,
) -> bool {
    loop {
        match sender.send_timeout(event, CHANNEL_RETRY) {
            Ok(()) => return true,
            Err(SendTimeoutError::Timeout(returned)) => {
                if cancelled.load(Ordering::Acquire) {
                    return false;
                }
                event = returned;
            }
            Err(SendTimeoutError::Disconnected(_)) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::time::Duration;

    use super::*;

    #[test]
    fn bounded_scanner_delivers_every_file() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..100 {
            std::fs::write(root.path().join(format!("file-{index}")), b"x")
                .expect("fixture file should be written");
        }

        let workers =
            WorkerPool::start(root.path().to_path_buf(), 2, 1).expect("workers should start");
        let mut names = HashSet::new();
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanEntry { path, .. } => {
                    if let Some(name) = path.file_name().and_then(|name| name.to_str())
                        && name.starts_with("file-")
                    {
                        names.insert(name.to_string());
                    }
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanSkippedLink { .. } | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert_eq!(names.len(), 100);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn shutdown_unblocks_a_backpressured_scanner() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..100 {
            std::fs::write(root.path().join(format!("file-{index}")), b"x")
                .expect("fixture file should be written");
        }
        let workers =
            WorkerPool::start(root.path().to_path_buf(), 2, 1).expect("workers should start");
        workers.shutdown().expect("shutdown should unblock senders");
    }

    #[cfg(unix)]
    #[test]
    fn scanner_never_traverses_descendant_symlinks() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().expect("scan root should exist");
        let outside = tempfile::tempdir().expect("outside root should exist");
        std::fs::write(outside.path().join("secret"), b"x")
            .expect("outside file should be written");
        let link = root.path().join("linked");
        symlink(outside.path(), &link).expect("directory link should be created");

        let workers =
            WorkerPool::start(root.path().to_path_buf(), 1, 16).expect("workers should start");
        let mut skipped_link = false;
        let mut traversed_secret = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanSkippedLink { path } => skipped_link |= path == link,
                WorkerEvent::ScanEntry { path, .. } => {
                    traversed_secret |= path.file_name().is_some_and(|name| name == "secret");
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(skipped_link);
        assert!(!traversed_secret);
        workers.shutdown().expect("workers should stop");
    }
}
