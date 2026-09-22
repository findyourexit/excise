mod scan_coordinator_core;
pub(crate) use scan_coordinator_core::*;

use std::fmt;
use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender, bounded};

use crate::scan_session::ScanSessionId;
const COORDINATOR_COMMAND_CAPACITY: usize = 64;

/// Failure while communicating with the private session coordinator actor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum SessionCoordinatorError {
    Disconnected,
    GenerationMismatch,
}

impl fmt::Display for SessionCoordinatorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disconnected => formatter.write_str("scan coordinator stopped"),
            Self::GenerationMismatch => {
                formatter.write_str("scan coordinator cannot move to an older scan generation")
            }
        }
    }
}

impl std::error::Error for SessionCoordinatorError {}

/// Bounded client for the one actor that owns a session's work ledger.
///
/// Scanner workers, the UI owner loop, and deletion lanes communicate through
/// this handle; none receives direct mutable access to queue or lease state.
#[derive(Clone)]
pub(crate) struct SessionCoordinator {
    inner: Arc<SessionCoordinatorInner>,
}

struct SessionCoordinatorInner {
    commands: Sender<CoordinatorCommand>,
    join: Mutex<Option<thread::JoinHandle<()>>>,
}

impl fmt::Debug for SessionCoordinator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionCoordinator")
    }
}

enum CoordinatorCommand {
    Register {
        key: WorkKey,
        priority: WorkPriority,
        reply: Sender<ScheduleOutcome>,
    },
    LeaseExact {
        key: WorkKey,
        reply: Sender<Option<WorkLease>>,
    },
    LeaseNextScan {
        reply: Sender<Option<WorkLease>>,
    },
    Acquire {
        kind: WorkKind,
        path: RelativePath,
        priority: WorkPriority,
        reply: Sender<Option<WorkLease>>,
    },
    Finish {
        lease: WorkLease,
        completion: WorkCompletion,
        reply: Sender<CompletionOutcome>,
    },
    Requeue {
        lease: WorkLease,
        reply: Sender<RequeueOutcome>,
    },
    Focus(RelativePath),
    AdvanceGeneration {
        generation: ScanGeneration,
        reply: Sender<Result<(), SessionCoordinatorError>>,
    },
    CancelScanWork,
    FailScanWork,
    InvalidateScanWork,
    Snapshot(Sender<SchedulerSnapshot>),
    Shutdown,
}

impl SessionCoordinator {
    /// Starts the private owner actor for one scan session.
    pub(crate) fn start(
        session: ScanSessionId,
        generation: ScanGeneration,
    ) -> Result<Self, std::io::Error> {
        let (commands, receiver) = bounded(COORDINATOR_COMMAND_CAPACITY);
        let join = thread::Builder::new()
            .name("excise-session-coordinator".to_string())
            .spawn(move || run_actor(receiver, ScanCoordinator::new(session, generation)))?;
        Ok(Self {
            inner: Arc::new(SessionCoordinatorInner {
                commands,
                join: Mutex::new(Some(join)),
            }),
        })
    }

    pub(crate) fn register(
        &self,
        key: WorkKey,
        priority: WorkPriority,
    ) -> Result<ScheduleOutcome, SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::Register {
            key,
            priority,
            reply,
        })?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }

    pub(crate) fn lease_exact(
        &self,
        key: WorkKey,
    ) -> Result<Option<WorkLease>, SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::LeaseExact { key, reply })?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }

    pub(crate) fn lease_next_scan(&self) -> Result<Option<WorkLease>, SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::LeaseNextScan { reply })?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }

    pub(crate) fn acquire(
        &self,
        kind: WorkKind,
        path: RelativePath,
        priority: WorkPriority,
    ) -> Result<Option<WorkLease>, SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::Acquire {
            kind,
            path,
            priority,
            reply,
        })?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }

    pub(crate) fn finish(
        &self,
        lease: WorkLease,
        completion: WorkCompletion,
    ) -> Result<CompletionOutcome, SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::Finish {
            lease,
            completion,
            reply,
        })?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }

    pub(crate) fn requeue(
        &self,
        lease: WorkLease,
    ) -> Result<RequeueOutcome, SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::Requeue { lease, reply })?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }

    pub(crate) fn focus(&self, path: RelativePath) {
        let _ = self.send(CoordinatorCommand::Focus(path));
    }

    pub(crate) fn advance_generation(
        &self,
        generation: ScanGeneration,
    ) -> Result<(), SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::AdvanceGeneration { generation, reply })?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)?
    }

    pub(crate) fn cancel_scan_work(&self) {
        let _ = self.send(CoordinatorCommand::CancelScanWork);
    }

    pub(crate) fn fail_scan_work(&self) {
        let _ = self.send(CoordinatorCommand::FailScanWork);
    }

    pub(crate) fn invalidate_scan_work(&self) {
        let _ = self.send(CoordinatorCommand::InvalidateScanWork);
    }

    pub(crate) fn snapshot(&self) -> Result<SchedulerSnapshot, SessionCoordinatorError> {
        let (reply, response) = bounded(1);
        self.send(CoordinatorCommand::Snapshot(reply))?;
        response
            .recv()
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }

    fn send(&self, command: CoordinatorCommand) -> Result<(), SessionCoordinatorError> {
        self.inner
            .commands
            .send(command)
            .map_err(|_| SessionCoordinatorError::Disconnected)
    }
}

impl Drop for SessionCoordinatorInner {
    fn drop(&mut self) {
        let _ = self.commands.send(CoordinatorCommand::Shutdown);
        if let Some(join) = self
            .join
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = join.join();
        }
    }
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the actor must own its receiving endpoint so its shutdown disconnects every sender"
)]
fn run_actor(receiver: Receiver<CoordinatorCommand>, mut coordinator: ScanCoordinator) {
    while let Ok(command) = receiver.recv() {
        match command {
            CoordinatorCommand::Register {
                key,
                priority,
                reply,
            } => {
                let _ = reply.send(coordinator.schedule(key, priority));
            }
            CoordinatorCommand::LeaseExact { key, reply } => {
                let _ = reply.send(coordinator.lease_exact(&key));
            }
            CoordinatorCommand::LeaseNextScan { reply } => {
                let _ = reply.send(coordinator.lease_next_scan());
            }
            CoordinatorCommand::Acquire {
                kind,
                path,
                priority,
                reply,
            } => {
                let key = WorkKey::new(coordinator.session(), coordinator.generation(), kind, path);
                let lease = match coordinator.schedule(key.clone(), priority) {
                    ScheduleOutcome::Enqueued
                    | ScheduleOutcome::PriorityRaised
                    | ScheduleOutcome::AlreadyPending => coordinator.lease_exact(&key),
                    ScheduleOutcome::AlreadyLeased
                    | ScheduleOutcome::StaleSession
                    | ScheduleOutcome::StaleGeneration => None,
                };
                let _ = reply.send(lease);
            }
            CoordinatorCommand::Finish {
                lease,
                completion,
                reply,
            } => {
                let _ = reply.send(coordinator.finish(&lease, completion));
            }
            CoordinatorCommand::Requeue { lease, reply } => {
                let _ = reply.send(coordinator.requeue(&lease));
            }
            CoordinatorCommand::Focus(path) => coordinator.focus(path),
            CoordinatorCommand::AdvanceGeneration { generation, reply } => {
                let result = if coordinator.generation() == generation
                    || coordinator.advance_to(generation)
                {
                    Ok(())
                } else {
                    Err(SessionCoordinatorError::GenerationMismatch)
                };
                let _ = reply.send(result);
            }
            CoordinatorCommand::CancelScanWork => coordinator.cancel_scan_work(),
            CoordinatorCommand::FailScanWork => coordinator.fail_scan_work(),
            CoordinatorCommand::InvalidateScanWork => coordinator.invalidate_scan_work(),
            CoordinatorCommand::Snapshot(reply) => {
                let _ = reply.send(coordinator.snapshot());
            }
            CoordinatorCommand::Shutdown => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> RelativePath {
        RelativePath::from_path(std::path::Path::new(value))
            .expect("fixture path should be relative")
    }

    #[test]
    fn session_actor_retains_deletion_work_across_scan_generation_advance() {
        let session = ScanSessionId::from_bytes([9; 16]);
        let coordinator = SessionCoordinator::start(session, ScanGeneration::initial())
            .expect("coordinator actor should start");
        let scan = WorkKey::new(
            session,
            ScanGeneration::initial(),
            WorkKind::EnumerateDirectory,
            path("scan"),
        );
        assert_eq!(
            coordinator
                .register(scan.clone(), WorkPriority::Background)
                .expect("scan work should register"),
            ScheduleOutcome::Enqueued
        );
        let scan_lease = coordinator
            .lease_exact(scan)
            .expect("actor should respond")
            .expect("scan work should lease");
        let deletion = coordinator
            .acquire(
                WorkKind::PlanDeletion,
                path("delete"),
                WorkPriority::Foreground,
            )
            .expect("deletion work should register")
            .expect("deletion work should lease");

        coordinator
            .advance_generation(ScanGeneration::from_value(2))
            .expect("newer generation should activate across retired work");
        assert_eq!(
            coordinator
                .finish(scan_lease, WorkCompletion::Succeeded)
                .expect("actor should respond"),
            CompletionOutcome::StaleLease
        );
        assert_eq!(
            coordinator
                .finish(deletion, WorkCompletion::Succeeded)
                .expect("actor should respond"),
            CompletionOutcome::Accepted
        );
    }

    #[test]
    fn session_actor_tracks_reducer_and_refresh_leases() {
        let session = ScanSessionId::from_bytes([3; 16]);
        let coordinator = SessionCoordinator::start(session, ScanGeneration::initial())
            .expect("coordinator actor should start");
        let reducer = coordinator
            .acquire(
                WorkKind::ReduceRun,
                RelativePath::root(),
                WorkPriority::Reducer,
            )
            .expect("reducer work should register")
            .expect("reducer work should lease");
        assert_eq!(
            coordinator
                .finish(reducer, WorkCompletion::Succeeded)
                .expect("actor should respond"),
            CompletionOutcome::Accepted
        );
        coordinator
            .advance_generation(ScanGeneration::from_value(1))
            .expect("next generation should activate");
        let refresh = coordinator
            .acquire(
                WorkKind::RefreshSubtree,
                RelativePath::root(),
                WorkPriority::Foreground,
            )
            .expect("refresh work should register")
            .expect("refresh work should lease");
        assert_eq!(
            coordinator
                .finish(refresh, WorkCompletion::Cancelled)
                .expect("actor should respond"),
            CompletionOutcome::Accepted
        );
    }
}
