use std::fs::Metadata;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, SendTimeoutError, Sender, bounded};

use crate::deletion::{
    DeletionPlan, DeletionPlanError, DeletionReport, build_plan_cancellable,
    build_plan_cancellable_with_root_identity, execute_plan, revalidate_plan_cancellable,
};
use crate::error::AppError;
use crate::native_path::NativeIdentity;
use crate::state::FileToDelete;

use super::scanner::{self, ScannerOptions};

const CHANNEL_RETRY: Duration = Duration::from_millis(25);

pub struct ScannedEntry {
    pub metadata: Metadata,
    pub path: PathBuf,
    pub identity: NativeIdentity,
}

pub(super) enum WorkerEvent {
    ScanBatch {
        entries: Vec<ScannedEntry>,
    },
    ScanDirectoryComplete {
        path: PathBuf,
    },
    ScanUnscanned {
        path: PathBuf,
        reason: crate::model::UnscannedReason,
    },
    ScanFailed {
        path: Option<PathBuf>,
        message: String,
    },
    ScanFinished {
        cancelled: bool,
    },
    DeletionPlanned {
        target_node_id: crate::model::NodeId,
        result: Result<Box<DeletionPlan>, DeletionPlanError>,
    },
    DeletionRevalidated {
        target_node_id: crate::model::NodeId,
        result: Result<Box<DeletionPlan>, (Box<DeletionPlan>, DeletionPlanError)>,
    },
    DeletionFinished {
        report: DeletionReport,
    },
}

enum WorkerCommand {
    PlanDeletion {
        maximum_bytes: usize,
        target: FileToDelete,
        reduced_guardrails: bool,
    },
    RevalidateDeletion(DeletionPlan),
    Rescan(ScannerOptions),
    ExecuteDeletion(DeletionPlan),
}

pub struct WorkerPool {
    events: Receiver<WorkerEvent>,
    commands: Sender<WorkerCommand>,
    cancelled: Arc<AtomicBool>,
    deletion_plan_cancelled: Arc<AtomicBool>,
    deletion_soft_cancelled: Arc<AtomicBool>,
    rescan_cancelled: Arc<AtomicBool>,
    scanner_handle: thread::JoinHandle<()>,
    deletion_handle: thread::JoinHandle<()>,
}

impl WorkerPool {
    pub fn start(scanner_options: ScannerOptions, event_capacity: usize) -> Result<Self, AppError> {
        let (event_sender, events) = bounded(event_capacity);
        let (commands, command_receiver) = bounded(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let deletion_plan_cancelled = Arc::new(AtomicBool::new(false));
        let rescan_cancelled = Arc::new(AtomicBool::new(false));
        let deletion_soft_cancelled = Arc::new(AtomicBool::new(false));
        let scan_root = scanner_options.root.clone();
        let scan_root_identity = scanner_options.root_identity.clone();

        let scanner = scanner::spawn(scanner_options, event_sender.clone(), cancelled.clone())
            .map_err(|error| AppError::io("could not spawn scanner worker", error))?;

        let worker_cancelled = cancelled.clone();
        let worker_plan_cancelled = deletion_plan_cancelled.clone();
        let worker_rescan_cancelled = rescan_cancelled.clone();
        let worker_soft_cancelled = deletion_soft_cancelled.clone();
        let deletion = match thread::Builder::new()
            .name("excise-deletion".to_string())
            .spawn(move || {
                deletion_worker(
                    &scan_root,
                    scan_root_identity.as_ref(),
                    &command_receiver,
                    &event_sender,
                    &worker_plan_cancelled,
                    &worker_soft_cancelled,
                    &worker_rescan_cancelled,
                    &worker_cancelled,
                );
            }) {
            Ok(handle) => handle,
            Err(error) => {
                cancelled.store(true, Ordering::Release);
                drop(events);
                scanner
                    .join()
                    .map_err(|_| AppError::Worker("scanner thread panicked".to_string()))?;
                return Err(AppError::io("could not spawn deletion worker", error));
            }
        };

        Ok(Self {
            events,
            commands,
            cancelled,
            deletion_plan_cancelled,
            deletion_soft_cancelled,
            rescan_cancelled,
            scanner_handle: scanner,
            deletion_handle: deletion,
        })
    }

    #[must_use]
    pub const fn events(&self) -> &Receiver<WorkerEvent> {
        &self.events
    }

    pub fn request_deletion_plan(
        &self,
        target: FileToDelete,
        reduced_guardrails: bool,
        maximum_bytes: usize,
    ) -> Result<(), AppError> {
        self.deletion_plan_cancelled.store(false, Ordering::Release);
        self.commands
            .send(WorkerCommand::PlanDeletion {
                target,
                maximum_bytes,
                reduced_guardrails,
            })
            .map_err(|_| AppError::Worker("deletion worker disconnected".to_string()))
    }

    pub fn execute_deletion(&self, plan: DeletionPlan) -> Result<(), AppError> {
        self.commands
            .send(WorkerCommand::ExecuteDeletion(plan))
            .map_err(|_| AppError::Worker("deletion worker disconnected".to_string()))
    }

    pub fn revalidate_deletion(&self, plan: DeletionPlan) -> Result<(), AppError> {
        self.deletion_soft_cancelled.store(false, Ordering::Release);
        self.commands
            .send(WorkerCommand::RevalidateDeletion(plan))
            .map_err(|_| AppError::Worker("deletion worker disconnected".to_string()))
    }

    pub fn soft_cancel_deletion(&self) {
        self.deletion_soft_cancelled.store(true, Ordering::Release);
    }
    pub fn cancel_deletion_plan(&self) {
        self.deletion_plan_cancelled.store(true, Ordering::Release);
    }
    pub fn resume_deletion(&self) {
        self.deletion_soft_cancelled.store(false, Ordering::Release);
    }

    pub fn request_rescan(&self, options: ScannerOptions) -> Result<(), AppError> {
        self.rescan_cancelled.store(false, Ordering::Release);
        self.commands
            .send(WorkerCommand::Rescan(options))
            .map_err(|_| AppError::Worker("rescan worker disconnected".to_string()))
    }

    pub fn cancel_rescan(&self) {
        self.rescan_cancelled.store(true, Ordering::Release);
    }

    pub fn shutdown(self) -> Result<(), AppError> {
        self.stop(false)
    }

    pub fn hard_shutdown(self) -> Result<(), AppError> {
        self.stop(true)
    }

    fn stop(self, detach_deletion: bool) -> Result<(), AppError> {
        self.cancelled.store(true, Ordering::Release);
        self.deletion_plan_cancelled.store(true, Ordering::Release);
        self.deletion_soft_cancelled.store(true, Ordering::Release);
        self.rescan_cancelled.store(true, Ordering::Release);
        drop(self.events);
        drop(self.commands);
        if detach_deletion {
            drop(self.scanner_handle);
            drop(self.deletion_handle);
            return Ok(());
        }
        self.scanner_handle
            .join()
            .map_err(|_| AppError::Worker("scanner thread panicked".to_string()))?;
        self.deletion_handle
            .join()
            .map_err(|_| AppError::Worker("deletion thread panicked".to_string()))
    }
}
#[allow(
    clippy::too_many_arguments,
    reason = "The worker receives each independent bounded command, cancellation, and filesystem capability explicitly."
)]
fn deletion_worker(
    scan_root: &std::path::Path,
    scan_root_identity: Option<&NativeIdentity>,
    commands: &Receiver<WorkerCommand>,
    sender: &Sender<WorkerEvent>,
    plan_cancelled: &AtomicBool,
    soft_cancelled: &AtomicBool,
    rescan_cancelled: &Arc<AtomicBool>,
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
        let event = match command {
            WorkerCommand::PlanDeletion {
                target,
                reduced_guardrails,
                maximum_bytes,
            } => {
                let target_node_id = target.node_id;
                let result = if let Some(identity) = scan_root_identity {
                    build_plan_cancellable_with_root_identity(
                        scan_root,
                        identity.clone(),
                        target,
                        reduced_guardrails,
                        plan_cancelled,
                        maximum_bytes,
                    )
                } else {
                    build_plan_cancellable(
                        scan_root,
                        target,
                        reduced_guardrails,
                        plan_cancelled,
                        maximum_bytes,
                    )
                }
                .map(Box::new);
                WorkerEvent::DeletionPlanned {
                    target_node_id,
                    result,
                }
            }
            WorkerCommand::RevalidateDeletion(plan) => {
                let target_node_id = plan.target.node_id;
                let result = match revalidate_plan_cancellable(scan_root, &plan, soft_cancelled) {
                    Ok(()) => Ok(Box::new(plan)),
                    Err(error) => Err((Box::new(plan), error)),
                };
                WorkerEvent::DeletionRevalidated {
                    target_node_id,
                    result,
                }
            }
            WorkerCommand::Rescan(options) => {
                scanner::run(options, sender, rescan_cancelled.as_ref());
                continue;
            }
            WorkerCommand::ExecuteDeletion(plan) => WorkerEvent::DeletionFinished {
                report: execute_plan(scan_root, plan, soft_cancelled, cancelled),
            },
        };
        if !send_event(sender, event, cancelled) {
            return;
        }
    }
}

pub(super) fn send_event(
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

    use crate::model::UnscannedReason;

    use super::*;

    fn options(root: &std::path::Path, threads: usize) -> ScannerOptions {
        ScannerOptions {
            root: root.to_path_buf(),
            root_identity: None,
            threads,
            cross_filesystems: false,
            exclusions: Vec::new(),
            internal_paths: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn single_file_plan(root: &std::path::Path) -> (std::path::PathBuf, DeletionPlan) {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::UNIX_EPOCH;

        use crate::deletion::{PlannedKind, PlannedSnapshot, ReviewedEntry, build_plan};
        use crate::native_path::identity_for;
        use crate::state::tiles::FileType;

        let path = root.join("target");
        std::fs::write(&path, b"payload").expect("target should be written");
        let metadata = std::fs::symlink_metadata(&path).expect("target metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("target identity should be readable")
            .expect("target identity should be available");
        let snapshot = PlannedSnapshot {
            identity: identity.clone(),
            kind: PlannedKind::File,
            apparent_bytes: u128::from(metadata.len()),
            allocated_bytes: Some(u128::from(metadata.blocks()).saturating_mul(512)),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        };
        let target = FileToDelete {
            node_id: crate::model::NodeId(1),
            synthetic: false,
            path_in_filesystem: root.to_path_buf(),
            path_to_file: vec![std::ffi::OsString::from("target")],
            file_type: FileType::File,
            num_descendants: None,
            size: snapshot.apparent_bytes,
            expected_snapshot: crate::model::EntrySnapshot {
                identity: Some(identity),
                kind: crate::model::NodeKind::File,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries: vec![ReviewedEntry {
                relative_path: std::path::PathBuf::from("target"),
                snapshot: snapshot.clone(),
            }],
        };
        let plan = build_plan(root, target, false).expect("deletion plan should build");
        (path, plan)
    }

    #[cfg(unix)]
    #[test]
    fn soft_cancel_wins_over_queued_revalidation_success() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let (path, plan) = single_file_plan(root.path());
        let workers = WorkerPool::start(options(root.path(), 1), 16).expect("workers should start");
        workers
            .revalidate_deletion(plan)
            .expect("revalidation should be queued");

        let plan = loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("worker should report revalidation")
            {
                WorkerEvent::DeletionRevalidated {
                    result: Ok(plan), ..
                } => break *plan,
                WorkerEvent::DeletionRevalidated { result: Err(_), .. } => {
                    panic!("revalidation should succeed")
                }
                _ => {}
            }
        };

        workers.soft_cancel_deletion();
        workers
            .execute_deletion(plan)
            .expect("execution should be queued after cancellation");
        let report = loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("worker should report deletion")
            {
                WorkerEvent::DeletionFinished { report } => break report,
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. } => {}
            }
        };
        assert!(report.soft_cancelled);
        assert_eq!(report.unattempted_entries(), 1);
        assert!(path.exists());
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn resume_deletion_clears_soft_cancel_before_execution() {
        let root = tempfile::tempdir().expect("deletion root should exist");
        let (path, plan) = single_file_plan(root.path());
        let workers = WorkerPool::start(options(root.path(), 1), 16).expect("workers should start");
        workers.soft_cancel_deletion();
        workers.resume_deletion();
        workers
            .execute_deletion(plan)
            .expect("execution should be queued after resuming");

        let report = loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("worker should report deletion")
            {
                WorkerEvent::DeletionFinished { report } => break report,
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. } => {}
            }
        };
        assert!(!report.soft_cancelled);
        assert_eq!(report.deleted_entries(), 1);
        assert!(!path.exists());
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn bounded_scanner_delivers_every_file() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..100 {
            std::fs::write(root.path().join(format!("file-{index}")), b"x")
                .expect("fixture file should be written");
        }

        let workers = WorkerPool::start(options(root.path(), 2), 1).expect("workers should start");
        let mut names = HashSet::new();
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanBatch { entries } => {
                    for entry in entries {
                        if let Some(name) = entry.path.file_name().and_then(|name| name.to_str())
                            && name.starts_with("file-")
                        {
                            names.insert(name.to_string());
                        }
                    }
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert_eq!(names.len(), 100);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn focused_rescan_reuses_bounded_worker_channel() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let file = root.path().join("file");
        std::fs::write(&file, b"x").expect("fixture should be written");
        let scanner_options = options(root.path(), 1);
        let workers = WorkerPool::start(scanner_options.clone(), 1).expect("workers should start");

        for pass in 0..2 {
            if pass == 1 {
                workers
                    .request_rescan(scanner_options.clone())
                    .expect("focused rescan should start");
            }
            let mut saw_file = false;
            loop {
                match workers
                    .events()
                    .recv_timeout(Duration::from_secs(5))
                    .expect("scan pass should complete")
                {
                    WorkerEvent::ScanBatch { entries } => {
                        saw_file |= entries.iter().any(|entry| entry.path == file);
                    }
                    WorkerEvent::ScanFinished { cancelled: false } => break,
                    WorkerEvent::ScanFinished { cancelled: true } => {
                        panic!("scan pass was cancelled")
                    }
                    WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                    WorkerEvent::ScanDirectoryComplete { .. }
                    | WorkerEvent::ScanUnscanned { .. }
                    | WorkerEvent::DeletionPlanned { .. }
                    | WorkerEvent::DeletionRevalidated { .. }
                    | WorkerEvent::DeletionFinished { .. } => {}
                }
            }
            assert!(saw_file);
        }
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn deep_wide_scan_completes_with_bounded_queue() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let mut deepest = root.path().to_path_buf();
        for depth in 0..300 {
            for sibling in 0..9 {
                std::fs::create_dir(deepest.join(format!("s{depth}-{sibling}")))
                    .expect("sibling directory should be created");
            }
            deepest.push("d");
            std::fs::create_dir(&deepest).expect("deep directory should be created");
        }
        let marker = deepest.join("marker");
        std::fs::write(&marker, b"x").expect("deep marker should be written");
        let workers = WorkerPool::start(options(root.path(), 1), 1).expect("workers should start");
        let mut found = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(10))
                .expect("deep scan should complete")
            {
                WorkerEvent::ScanBatch { entries } => {
                    found |= entries.iter().any(|entry| entry.path == marker);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(found);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn bounded_directory_queue_delivers_wide_tree() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..64 {
            let directory = root.path().join(format!("directory-{index}"));
            std::fs::create_dir(&directory).expect("fixture directory should be created");
            std::fs::write(directory.join("file"), b"x").expect("fixture file should be written");
        }

        let workers = WorkerPool::start(options(root.path(), 1), 1).expect("workers should start");
        let mut files = HashSet::new();
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("wide scan should complete")
            {
                WorkerEvent::ScanBatch { entries } => {
                    for entry in entries {
                        if entry.path.file_name().is_some_and(|name| name == "file") {
                            files.insert(entry.path);
                        }
                    }
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert_eq!(files.len(), 64);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn shutdown_unblocks_a_backpressured_scanner() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..100 {
            std::fs::write(root.path().join(format!("file-{index}")), b"x")
                .expect("fixture file should be written");
        }
        let workers = WorkerPool::start(options(root.path(), 2), 1).expect("workers should start");
        workers.shutdown().expect("shutdown should unblock senders");
    }

    #[test]
    fn scanner_prunes_configured_exclusions() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let ignored = root.path().join("ignored");
        std::fs::create_dir(&ignored).expect("ignored directory should be created");
        let secret = ignored.join("secret");
        std::fs::write(&secret, b"x").expect("ignored file should be written");
        let mut scanner_options = options(root.path(), 1);
        scanner_options.exclusions = vec!["ignored/".to_string()];

        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        let mut excluded_directory = false;
        let mut traversed_secret = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("excluded scan should complete")
            {
                WorkerEvent::ScanUnscanned { path, reason } => {
                    excluded_directory |= path == ignored
                        && reason == UnscannedReason::Excluded("ignored/".to_string());
                }
                WorkerEvent::ScanBatch { entries } => {
                    traversed_secret |= entries.iter().any(|entry| entry.path == secret);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(excluded_directory);
        assert!(!traversed_secret);
        workers.shutdown().expect("workers should stop");
    }

    #[test]
    fn scanner_skips_only_explicit_internal_paths() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let user_session = root.path().join(".excise-session-user-data");
        let active_spill = root.path().join(".excise-session-active");
        std::fs::create_dir(&user_session).expect("user-owned session-like directory should exist");
        std::fs::create_dir(&active_spill).expect("active spill directory should exist");
        let user_file = user_session.join("user-file");
        let spill_file = active_spill.join("spill-file");
        std::fs::write(&user_file, b"user data").expect("user fixture should be written");
        std::fs::write(&spill_file, b"spill data").expect("spill fixture should be written");

        let mut scanner_options = options(root.path(), 1);
        scanner_options.internal_paths = vec![active_spill.clone()];
        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        let mut saw_spill_directory = false;
        let mut saw_user_directory = false;
        let mut saw_user_file = false;
        let mut saw_spill_file = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should complete")
            {
                WorkerEvent::ScanBatch { entries } => {
                    saw_user_directory |= entries.iter().any(|entry| entry.path == user_session);
                    saw_user_file |= entries.iter().any(|entry| entry.path == user_file);
                    saw_spill_file |= entries.iter().any(|entry| entry.path == spill_file);
                    saw_spill_directory |= entries.iter().any(|entry| entry.path == active_spill);
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }

        assert!(!saw_spill_directory);
        assert!(saw_user_directory);
        assert!(saw_user_file);
        assert!(!saw_spill_file);
        workers.shutdown().expect("workers should stop");
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

        let workers = WorkerPool::start(options(root.path(), 1), 16).expect("workers should start");
        let mut skipped_link = false;
        let mut traversed_secret = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanUnscanned { path, .. } => skipped_link |= path == link,
                WorkerEvent::ScanBatch { entries } => {
                    traversed_secret |= entries
                        .iter()
                        .any(|entry| entry.path.file_name().is_some_and(|name| name == "secret"));
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => panic!("scan was cancelled"),
                WorkerEvent::ScanFailed { message, .. } => panic!("scan failed: {message}"),
                WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(skipped_link);
        assert!(!traversed_secret);
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn scanner_rejects_replaced_root_before_traversal() {
        use crate::native_path::identity_for;
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("scan parent should exist");
        let scan_root = parent.path().join("scan-root");
        let original = parent.path().join("original-root");
        let outside = parent.path().join("outside-root");
        std::fs::create_dir(&scan_root).expect("scan root should be created");
        std::fs::write(scan_root.join("original"), b"original")
            .expect("original fixture should be written");
        let metadata = std::fs::symlink_metadata(&scan_root).expect("root metadata should exist");
        let identity = identity_for(&scan_root, &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a symbolic link");
        std::fs::rename(&scan_root, &original).expect("original root should be displaced");
        std::fs::create_dir(&outside).expect("replacement root should be created");
        std::fs::write(outside.join("replacement"), b"replacement")
            .expect("replacement fixture should be written");
        symlink(&outside, &scan_root).expect("replacement symlink should be created");

        let mut scanner_options = options(&scan_root, 1);
        scanner_options.root_identity = Some(identity);
        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        let mut failed = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanFailed { message, .. } => {
                    failed = message.contains("replaced") || message.contains("changed");
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => {
                    panic!("replaced root should be rejected, not cancelled")
                }
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(failed);
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn scanner_stops_before_following_a_replaced_root() {
        use crate::native_path::identity_for;
        use std::os::unix::fs::symlink;

        let parent = tempfile::tempdir().expect("scan parent should exist");
        let scan_root = parent.path().join("scan-root");
        let original = parent.path().join("original-root");
        let outside = parent.path().join("outside-root");
        std::fs::create_dir(&scan_root).expect("scan root should be created");
        std::fs::create_dir(&outside).expect("replacement root should be created");
        for index in 0..256 {
            std::fs::write(scan_root.join(format!("original-{index}")), b"original")
                .expect("original fixture should be written");
        }
        std::fs::write(outside.join("replacement"), b"replacement")
            .expect("replacement fixture should be written");
        let metadata = std::fs::symlink_metadata(&scan_root).expect("root metadata should exist");
        let identity = identity_for(&scan_root, &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a symbolic link");

        let mut scanner_options = options(&scan_root, 1);
        scanner_options.root_identity = Some(identity);
        let workers = WorkerPool::start(scanner_options, 1).expect("workers should start");
        let mut replaced = false;
        let mut saw_root_change = false;
        let mut saw_replacement = false;
        loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("scanner should produce completion")
            {
                WorkerEvent::ScanBatch { entries } => {
                    saw_replacement |= entries
                        .iter()
                        .any(|entry| entry.path == outside.join("replacement"));
                    if !replaced {
                        std::fs::rename(&scan_root, &original)
                            .expect("original root should be displaced");
                        symlink(&outside, &scan_root)
                            .expect("replacement symlink should be created");
                        replaced = true;
                    }
                }
                WorkerEvent::ScanFailed { message, .. } => {
                    saw_root_change |= message.contains("during traversal");
                }
                WorkerEvent::ScanFinished { cancelled: false } => break,
                WorkerEvent::ScanFinished { cancelled: true } => {
                    panic!("root replacement should be uncertain, not cancellation")
                }
                WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionRevalidated { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
        assert!(replaced, "test must replace root after the first batch");
        assert!(
            saw_root_change,
            "root replacement should emit an explicit failure"
        );
        assert!(
            !saw_replacement,
            "scanner must not follow the replacement root"
        );
        workers.shutdown().expect("workers should stop");
    }

    #[cfg(unix)]
    #[test]
    fn final_plan_revalidation_runs_in_cancellable_worker() {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::UNIX_EPOCH;

        use crate::deletion::{
            ConfirmationChallenge, DeletionPlan, PlannedEntry, PlannedKind, PlannedSnapshot,
            ReviewedEntry, current_scan_root_identity,
        };
        use crate::native_path::identity_for;
        use crate::state::FileToDelete;
        use crate::state::tiles::FileType;

        let root = tempfile::tempdir().expect("scan root should exist");
        let path = root.path().join("target");
        std::fs::write(&path, b"original").expect("target should be written");
        let metadata = std::fs::symlink_metadata(&path).expect("target metadata should exist");
        let identity = identity_for(&path, &metadata)
            .expect("target identity should be readable")
            .expect("target identity should be available");
        let snapshot = PlannedSnapshot {
            identity: identity.clone(),
            kind: PlannedKind::File,
            apparent_bytes: u128::from(metadata.len()),
            allocated_bytes: Some(u128::from(metadata.blocks()).saturating_mul(512)),
            modified_nanos: metadata
                .modified()
                .ok()
                .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos()),
        };
        let target = FileToDelete {
            node_id: crate::model::NodeId(1),
            synthetic: false,
            path_in_filesystem: root.path().to_path_buf(),
            path_to_file: vec![std::ffi::OsString::from("target")],
            file_type: FileType::File,
            num_descendants: None,
            size: snapshot.apparent_bytes,
            expected_snapshot: crate::model::EntrySnapshot {
                identity: Some(identity),
                kind: crate::model::NodeKind::File,
                apparent_bytes: snapshot.apparent_bytes,
                allocated_bytes: snapshot.allocated_bytes,
                modified_nanos: snapshot.modified_nanos,
            },
            reviewed_entries: vec![ReviewedEntry {
                relative_path: std::path::PathBuf::from("target"),
                snapshot: snapshot.clone(),
            }],
        };
        let root_identity =
            current_scan_root_identity(root.path()).expect("scan root identity should be readable");
        let plan = DeletionPlan {
            target,
            root_relative_path: std::path::PathBuf::from("target"),
            scan_root_identity: root_identity.clone(),
            entries: vec![PlannedEntry {
                relative_path: std::path::PathBuf::from("target"),
                snapshot,
            }],
            challenge: ConfirmationChallenge::ConfirmFile,
            apparent_bytes: 8,
            estimated_bytes: 1,
        };
        let mut scanner_options = options(root.path(), 1);
        scanner_options.root_identity = Some(root_identity);
        let workers = WorkerPool::start(scanner_options, 16).expect("workers should start");
        std::fs::write(&path, b"replacement-after-confirmation")
            .expect("replacement should be written");
        workers
            .revalidate_deletion(plan)
            .expect("revalidation should be queued");

        let result = loop {
            match workers
                .events()
                .recv_timeout(Duration::from_secs(5))
                .expect("worker should report revalidation")
            {
                WorkerEvent::DeletionRevalidated { result, .. } => break result,
                WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanDirectoryComplete { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::ScanFailed { .. }
                | WorkerEvent::ScanFinished { .. }
                | WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        };
        let (returned, error) = result.expect_err("changed confirmation should be rejected");
        assert_eq!(returned.entries.len(), 1);
        assert!(error.is_changed());
        workers.shutdown().expect("workers should stop");
    }
    #[test]
    fn hard_shutdown_does_not_join_blocked_workers() {
        let (_event_sender, events) = bounded::<WorkerEvent>(1);
        let (commands, command_receiver) = bounded::<WorkerCommand>(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let deletion_soft_cancelled = Arc::new(AtomicBool::new(false));
        let rescan_cancelled = Arc::new(AtomicBool::new(false));
        let scanner_handle = thread::spawn(|| {});
        let deletion_handle = thread::spawn(|| thread::sleep(Duration::from_secs(2)));
        drop(command_receiver);
        let pool = WorkerPool {
            events,
            commands,
            cancelled: cancelled.clone(),
            deletion_plan_cancelled: Arc::new(AtomicBool::new(false)),
            deletion_soft_cancelled,
            rescan_cancelled,
            scanner_handle,
            deletion_handle,
        };

        let started = std::time::Instant::now();
        pool.hard_shutdown()
            .expect("hard shutdown should detach blocked workers");

        assert!(started.elapsed() < Duration::from_millis(500));
        assert!(cancelled.load(Ordering::Acquire));
    }
}
