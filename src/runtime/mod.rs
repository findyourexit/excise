mod clock;
mod worker;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use crossbeam_channel::{RecvTimeoutError, TryRecvError};
use crossterm::event::Event;
use ratatui::backend::Backend;

pub use clock::{Clock, SystemClock, VirtualClock};
use worker::{DeletionFailure, WorkerEvent, WorkerPool, prepare_deletion};

use crate::App;
use crate::animation::AnimationScheduler;
use crate::error::{AppError, ExitClass};
use crate::input::{InputCommand, InputEvent, InputSource, handle_keypress};
use crate::native_path::{NativeIdentity, safe_display_path};
use crate::outcome::{OperationOutcome, RunSummary};

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const IDLE_INPUT_WAIT: Duration = Duration::from_secs(60 * 60);
const LOADING_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const TRANSIENT_STATUS_DURATION: Duration = Duration::from_millis(250);
const MAX_INPUT_BATCH: usize = 32;
const MAX_WORKER_BATCH: usize = 128;

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct RuntimeSettings {
    pub root: PathBuf,
    pub scan_threads: usize,
    pub event_capacity: usize,
    pub apparent_size: bool,
    pub disable_delete_confirmation: bool,
    pub reduced_motion: bool,
    pub monochrome: bool,
    pub animate_loading: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimedAction {
    ResetPathColor,
    UnflashSpace,
}

struct ScheduledAction {
    at: Duration,
    action: TimedAction,
}

#[allow(clippy::struct_excessive_bools)]
struct OwnerLoop<B>
where
    B: Backend,
{
    app: App<B>,
    input: Box<dyn InputSource>,
    workers: Option<WorkerPool>,
    clock: Box<dyn Clock>,
    animation: AnimationScheduler,
    settings: RuntimeSettings,
    summary: RunSummary,
    seen_identities: HashSet<NativeIdentity>,
    scan_active: bool,
    scan_cancelled: bool,
    cancelled_while_scanning: bool,
    hard_cancelled: bool,
    deletion_active: bool,
    timed_actions: Vec<ScheduledAction>,
    next_loading_frame: Duration,
}

/// # Errors
/// Returns a terminal, input, worker, or invariant error after shutting down owned workers.
pub fn run<B>(
    terminal_backend: B,
    input: Box<dyn InputSource>,
    settings: RuntimeSettings,
    clock: Box<dyn Clock>,
) -> Result<OperationOutcome<RunSummary>, AppError>
where
    B: Backend,
{
    let now = clock.now();
    let app = App::new(
        terminal_backend,
        settings.root.clone(),
        settings.apparent_size,
        settings.disable_delete_confirmation,
    )?;
    let workers = WorkerPool::start(
        settings.root.clone(),
        settings.scan_threads,
        settings.event_capacity,
    )?;
    let animation = AnimationScheduler::new(settings.reduced_motion, settings.monochrome, now);
    OwnerLoop {
        app,
        input,
        workers: Some(workers),
        clock,
        animation,
        settings,
        seen_identities: HashSet::new(),
        summary: RunSummary::default(),
        scan_active: true,
        scan_cancelled: false,
        cancelled_while_scanning: false,
        hard_cancelled: false,
        deletion_active: false,
        timed_actions: Vec::new(),
        next_loading_frame: now.saturating_add(LOADING_FRAME_INTERVAL),
    }
    .run()
}

impl<B> OwnerLoop<B>
where
    B: Backend,
{
    fn run(mut self) -> Result<OperationOutcome<RunSummary>, AppError> {
        let loop_result = self.run_loop();
        let shutdown_result = self
            .workers
            .take()
            .ok_or_else(|| AppError::Invariant("worker pool already stopped".to_string()))?
            .shutdown();
        let finish_result = self.app.finish();

        let outcome = loop_result?;
        shutdown_result?;
        finish_result?;
        Ok(outcome)
    }

    fn run_loop(&mut self) -> Result<OperationOutcome<RunSummary>, AppError> {
        self.render()?;
        while self.app.is_running {
            let mut did_work = self.process_input_batch()?;
            did_work |= self.process_worker_batch()?;
            did_work |= self.process_deadlines();
            self.update_animation_frame();
            did_work |= self.render()?;

            if !self.app.is_running {
                break;
            }
            if !did_work {
                let timeout = self.next_timeout();
                if self.input.poll(timeout)? {
                    self.process_one_input()?;
                    self.process_deadlines();
                    self.update_animation_frame();
                    self.render()?;
                }
            }
        }
        if !self.app.is_running && self.scan_active {
            self.cancelled_while_scanning = true;
        }

        let summary = self.summary.clone();
        if self.hard_cancelled {
            Ok(OperationOutcome::Cancelled {
                value: Some(summary),
                precise: false,
            })
        } else if self.scan_cancelled || self.cancelled_while_scanning {
            Ok(OperationOutcome::Cancelled {
                value: Some(summary),
                precise: true,
            })
        } else if summary.unreadable_entries > 0 {
            Ok(OperationOutcome::Uncertain {
                unreadable_entries: summary.unreadable_entries,
                value: summary,
            })
        } else {
            Ok(OperationOutcome::Exact(summary))
        }
    }
    fn process_input_batch(&mut self) -> Result<bool, AppError> {
        let mut processed = false;
        for _ in 0..MAX_INPUT_BATCH {
            if !self.input.poll(Duration::ZERO)? {
                break;
            }
            let boundary = self.process_one_input()?;
            processed = true;
            if boundary || !self.app.is_running {
                break;
            }
        }
        Ok(processed)
    }

    fn process_one_input(&mut self) -> Result<bool, AppError> {
        match self.input.read()? {
            InputEvent::Barrier => {
                self.render()?;
                self.wait_for_quiescence()?;
                Ok(true)
            }
            InputEvent::Terminal(Event::Resize(_, _)) => {
                self.app.reset_ui_mode();
                self.app.mark_dirty();
                Ok(false)
            }
            InputEvent::Terminal(event) => {
                let command = handle_keypress(&event, &mut self.app);
                self.handle_input_command(command)?;
                Ok(false)
            }
        }
    }

    fn handle_input_command(&mut self, command: InputCommand) -> Result<(), AppError> {
        let now = self.clock.now();
        match command {
            InputCommand::Navigation if self.app.ui_mode.allows_motion() => {
                self.animation.schedule_navigation();
                self.app.mark_dirty();
            }
            InputCommand::None | InputCommand::Navigation => {}
            InputCommand::PathError => {
                self.app.set_path_to_red();
                self.schedule(now, TimedAction::ResetPathColor, TRANSIENT_STATUS_DURATION);
            }
            InputCommand::Delete(target) => {
                if self.deletion_active {
                    return Ok(());
                }
                match prepare_deletion(target) {
                    Ok(request) => {
                        self.app.begin_deletion(&request.target);
                        self.workers()?.request_deletion(request)?;
                        self.deletion_active = true;
                    }
                    Err(error) => self.app.show_error(deletion_error_message(&error)),
                }
            }
            InputCommand::HardCancel => self.hard_cancelled = true,
        }
        if !self.app.ui_mode.allows_motion() {
            self.animation.cancel_all();
        }
        Ok(())
    }

    fn process_worker_batch(&mut self) -> Result<bool, AppError> {
        let mut processed = false;
        for _ in 0..MAX_WORKER_BATCH {
            let event = match self.workers()?.events().try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.scan_active || self.deletion_active {
                        return Err(AppError::Worker(
                            "worker event channel disconnected".to_string(),
                        ));
                    }
                    break;
                }
            };
            self.handle_worker_event(event);
            processed = true;
        }
        Ok(processed)
    }

    fn handle_worker_event(&mut self, event: WorkerEvent) {
        match event {
            WorkerEvent::ScanEntry {
                metadata,
                path,
                identity,
            } => {
                self.summary.scanned_entries += 1;
                if self.seen_identities.insert(identity) {
                    self.summary.identified_entries += 1;
                }
                self.app.add_entry_to_base_folder(&metadata, path);
            }
            WorkerEvent::ScanSkippedLink { path } => {
                self.summary.unreadable_entries += 1;
                self.summary.last_unreadable_path = Some(safe_display_path(&path).text);
                self.summary.last_worker_error = Some("symbolic link skipped".to_string());
                self.app.increment_failed_to_read();
            }
            WorkerEvent::ScanFailed { path, message } => {
                self.summary.unreadable_entries += 1;
                self.summary.last_unreadable_path =
                    path.as_deref().map(|path| safe_display_path(path).text);
                self.summary.last_worker_error = Some(message);
                self.app.increment_failed_to_read();
            }
            WorkerEvent::ScanFinished { cancelled } => {
                self.scan_active = false;
                self.scan_cancelled = cancelled;
                if !cancelled {
                    self.app.start_ui();
                    self.animation.schedule_state_change();
                }
            }
            WorkerEvent::DeletionFinished { request, result } => {
                self.deletion_active = false;
                let error = result.as_ref().err().map(deletion_error_message);
                if self.app.complete_deletion(&request.target, error) {
                    self.summary.deleted_entries += 1;
                    self.app.flash_space_freed();
                    self.schedule(
                        self.clock.now(),
                        TimedAction::UnflashSpace,
                        TRANSIENT_STATUS_DURATION,
                    );
                }
            }
        }
    }

    fn process_deadlines(&mut self) -> bool {
        let now = self.clock.now();
        let mut processed = false;
        let mut pending = Vec::with_capacity(self.timed_actions.len());
        for scheduled in self.timed_actions.drain(..) {
            if scheduled.at <= now {
                match scheduled.action {
                    TimedAction::ResetPathColor => self.app.reset_current_path_color(),
                    TimedAction::UnflashSpace => self.app.unflash_space_freed(),
                }
                processed = true;
            } else {
                pending.push(scheduled);
            }
        }
        self.timed_actions = pending;

        if self.settings.animate_loading && self.scan_active && now >= self.next_loading_frame {
            self.app.increment_loading_progress_indicator();
            self.app.render_and_update_board();
            self.next_loading_frame = now.saturating_add(LOADING_FRAME_INTERVAL);
            processed = true;
        }
        processed
    }

    fn update_animation_frame(&mut self) {
        if self
            .animation
            .next_frame_at()
            .is_some_and(|deadline| self.clock.now() >= deadline)
        {
            self.app.mark_dirty();
        }
    }

    fn render(&mut self) -> Result<bool, AppError> {
        self.app
            .render_if_dirty(&mut self.animation, self.clock.now())
    }

    fn schedule(&mut self, now: Duration, action: TimedAction, delay: Duration) {
        self.timed_actions
            .retain(|scheduled| scheduled.action != action);
        self.timed_actions.push(ScheduledAction {
            at: now.saturating_add(delay),
            action,
        });
    }

    fn next_timeout(&self) -> Duration {
        let now = self.clock.now();
        let mut timeout = if self.scan_active || self.deletion_active {
            WORKER_POLL_INTERVAL
        } else {
            IDLE_INPUT_WAIT
        };
        if self.settings.animate_loading && self.scan_active {
            timeout = timeout.min(self.next_loading_frame.saturating_sub(now));
        }
        if let Some(deadline) = self.animation.next_frame_at() {
            timeout = timeout.min(deadline.saturating_sub(now));
        }
        if let Some(deadline) = self.timed_actions.iter().map(|action| action.at).min() {
            timeout = timeout.min(deadline.saturating_sub(now));
        }
        timeout
    }

    fn wait_for_quiescence(&mut self) -> Result<(), AppError> {
        while self.scan_active || self.deletion_active {
            match self.workers()?.events().recv_timeout(WORKER_POLL_INTERVAL) {
                Ok(event) => {
                    self.handle_worker_event(event);
                    self.process_worker_batch()?;
                    self.render()?;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppError::Worker(
                        "worker event channel disconnected".to_string(),
                    ));
                }
            }
        }

        loop {
            let next_timed = self.timed_actions.iter().map(|action| action.at).min();
            let next_animation = self.animation.next_frame_at();
            let next = next_timed.into_iter().chain(next_animation).min();
            let Some(next) = next else {
                break;
            };
            if !self.clock.advance_to(next) {
                break;
            }
            self.process_deadlines();
            self.update_animation_frame();
            self.render()?;
        }
        Ok(())
    }

    fn workers(&self) -> Result<&WorkerPool, AppError> {
        self.workers
            .as_ref()
            .ok_or_else(|| AppError::Invariant("worker pool unavailable".to_string()))
    }
}

fn deletion_error_message(error: &DeletionFailure) -> String {
    match error {
        DeletionFailure::IdentityChanged => {
            "Deletion refused because the selected path changed identity".to_string()
        }
        DeletionFailure::SymbolicLink => {
            "Deletion refused because the selected path is a symbolic link".to_string()
        }
        DeletionFailure::Io(message) => message.clone(),
    }
}

#[must_use]
pub const fn outcome_exit_class(outcome: &OperationOutcome<RunSummary>) -> ExitClass {
    outcome.exit_class()
}
