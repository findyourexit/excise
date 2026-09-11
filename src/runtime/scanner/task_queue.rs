use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};

use super::task_spill::TaskSpill;
use crate::native_path::NativeIdentity;
use crate::temporary_storage::TemporaryStorage;

pub(super) const TASK_QUEUE_PER_WORKER: usize = 8;

#[derive(Clone)]
pub(super) struct DirectoryTask {
    pub(super) path: PathBuf,
    pub(super) identity: Option<NativeIdentity>,
}

pub(super) struct TaskQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
    capacity: usize,
    root: PathBuf,
}

struct QueueState {
    tasks: VecDeque<DirectoryTask>,
    spill: TaskSpill,
    pending: usize,
}

impl TaskQueue {
    pub(super) fn new(
        root: PathBuf,
        capacity: usize,
        temporary_storage: &TemporaryStorage,
    ) -> io::Result<(Self, Option<PathBuf>)> {
        let (spill, spill_path) = TaskSpill::new(temporary_storage)?;
        Ok((
            Self {
                state: Mutex::new(QueueState {
                    tasks: VecDeque::from([DirectoryTask {
                        path: root.clone(),
                        identity: None,
                    }]),
                    spill,
                    pending: 1,
                }),
                ready: Condvar::new(),
                capacity,
                root,
            },
            spill_path,
        ))
    }

    pub(super) fn take(
        &self,
        cancelled: &AtomicBool,
        failed: &AtomicBool,
        root_invalid: &AtomicBool,
    ) -> io::Result<Option<DirectoryTask>> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if cancelled.load(Ordering::Acquire)
                || failed.load(Ordering::Acquire)
                || root_invalid.load(Ordering::Acquire)
                || state.pending == 0
            {
                return Ok(None);
            }
            if let Some(task) = state.tasks.pop_front() {
                validate_task_path(&self.root, &task.path)?;
                return Ok(Some(task));
            }
            if let Some(task) = state.spill.take()? {
                validate_task_path(&self.root, &task.path)?;
                return Ok(Some(task));
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    pub(super) fn schedule(&self, task: DirectoryTask) -> io::Result<()> {
        validate_task_path(&self.root, &task.path)?;
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

    pub(super) fn complete(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending = state.pending.saturating_sub(1);
        self.ready.notify_all();
    }

    pub(super) fn cancel(&self) {
        self.ready.notify_all();
    }
}

fn validate_task_path(root: &Path, path: &Path) -> io::Result<()> {
    super::task_relative_path(root, path).map(|_| ())
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    #[cfg(windows)]
    use super::super::Exclusions;
    use super::super::{BATCH_SIZE, task_spill::SPILL_LENGTH_BYTES};
    use super::{DirectoryTask, TaskQueue};
    use crate::native_path::NativePath;
    use crate::temporary_storage::TemporaryStorage;

    #[test]
    fn task_queue_spills_overflow_without_growing_the_resident_queue() {
        let (queue, task_spill_path) =
            TaskQueue::new(PathBuf::from("/scan-root"), 1, &TemporaryStorage::default())
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
        assert_eq!(state.spill.pending(), BATCH_SIZE);
    }

    #[test]
    fn task_queue_enforces_a_total_spill_limit_and_reclaims_drained_records() {
        let root = PathBuf::from("/scan-root");
        let task = DirectoryTask {
            path: root.join("queued"),
            identity: None,
        };
        let payload = serde_json::to_vec(&(
            NativePath::new(task.path.clone()).encode(),
            task.identity.clone(),
        ))
        .expect("task fixture should serialize");
        let record_bytes = SPILL_LENGTH_BYTES
            .checked_add(u64::try_from(payload.len()).expect("task fixture length should fit"))
            .expect("task fixture record size should fit");
        let temporary_storage = TemporaryStorage::with_limit_bytes(record_bytes);
        let (queue, _) = TaskQueue::new(root.clone(), 1, &temporary_storage)
            .expect("scanner task queue should open");

        queue
            .schedule(task.clone())
            .expect("one spill record should fit the session limit");
        let error = queue
            .schedule(task)
            .expect_err("a second spill record must not exceed the session limit");
        assert_eq!(error.kind(), std::io::ErrorKind::StorageFull);
        assert!(error.to_string().contains("--temporary-storage-mib"));
        {
            let state = queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(state.tasks.len(), 1);
            assert_eq!(state.spill.pending(), 1);
        }
        assert_eq!(temporary_storage.used(), record_bytes);

        let cancelled = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        let root_task = queue
            .take(&cancelled, &failed, &root_invalid)
            .expect("resident task should be available")
            .expect("resident task should exist");
        assert_eq!(root_task.path, root);
        queue.complete();
        let queued_task = queue
            .take(&cancelled, &failed, &root_invalid)
            .expect("spilled task should be available")
            .expect("spilled task should exist");
        assert_eq!(queued_task.path, root.join("queued"));
        queue.complete();
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn task_spill_compacts_consumed_records_before_the_queue_drains() {
        let root = PathBuf::from("/scan-root");
        let task = DirectoryTask {
            path: root.join("queued"),
            identity: None,
        };
        let payload = serde_json::to_vec(&(
            NativePath::new(task.path.clone()).encode(),
            task.identity.clone(),
        ))
        .expect("task fixture should serialize");
        let record_bytes = SPILL_LENGTH_BYTES
            .checked_add(u64::try_from(payload.len()).expect("task fixture length should fit"))
            .expect("task fixture record size should fit");
        let temporary_storage = TemporaryStorage::with_limit_bytes(
            record_bytes
                .checked_mul(2)
                .expect("two task records should fit in the test limit"),
        );
        let (queue, _) = TaskQueue::new(root.clone(), 1, &temporary_storage)
            .expect("scanner task queue should open");
        queue
            .schedule(task.clone())
            .expect("first spill record should fit");
        queue
            .schedule(task.clone())
            .expect("second spill record should fit");

        let cancelled = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);
        queue
            .take(&cancelled, &failed, &root_invalid)
            .expect("resident task should be available")
            .expect("resident task should exist");
        queue.complete();
        queue
            .take(&cancelled, &failed, &root_invalid)
            .expect("first spilled task should be available")
            .expect("first spilled task should exist");
        queue.complete();
        assert_eq!(temporary_storage.used(), record_bytes);

        queue
            .schedule(task.clone())
            .expect("resident capacity should remain bounded");
        queue
            .schedule(task)
            .expect("compaction should free capacity for the next spill record");
        assert_eq!(
            temporary_storage.used(),
            record_bytes.checked_mul(2).expect("test limit should fit")
        );
        drop(queue);
        assert_eq!(temporary_storage.used(), 0);
    }

    #[test]
    fn task_queue_rejects_corrupt_spill_paths_outside_root() {
        let root = PathBuf::from("/scan-root");
        let (queue, _) = TaskQueue::new(root.clone(), 1, &TemporaryStorage::default())
            .expect("scanner task queue should open");
        let cancelled = AtomicBool::new(false);
        let failed = AtomicBool::new(false);
        let root_invalid = AtomicBool::new(false);

        assert!(
            queue
                .take(&cancelled, &failed, &root_invalid)
                .expect("initial scanner task should be available")
                .is_some()
        );
        queue.complete();
        {
            let mut state = queue
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state
                .spill
                .push(DirectoryTask {
                    path: PathBuf::from("/outside-root/secret"),
                    identity: None,
                })
                .expect("corrupt spill fixture should be writable");
            state.pending = 1;
        }

        let Err(error) = queue.take(&cancelled, &failed, &root_invalid) else {
            panic!("out-of-root spill path should be rejected")
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }
}
