mod clock;
mod scanner;
mod worker;

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(feature = "internal")]
use std::time::Instant;

use crossbeam_channel::{RecvTimeoutError, TryRecvError};
use crossterm::event::Event;
use ratatui::backend::Backend;

#[cfg(any(test, feature = "fuzzing", feature = "internal"))]
pub use clock::VirtualClock;
pub(crate) use clock::{Clock, SystemClock};
use worker::{DeletionWorkSubmissionError, ScannedEntry, WorkerEvent, WorkerPool};

use crate::App;
use crate::animation::AnimationScheduler;
use crate::app::ExitWork;
use crate::config::{CustomKeyBindings, KeyPreset, save_theme_preference};
use crate::deletion::{DeletionPlanError, DeletionReport};
use crate::error::{AppError, ExitClass};
use crate::input::{InputCommand, InputEvent, InputSource, handle_keypress};
use crate::native_path::{
    DECEPTIVE_DISPLAY_MARKER, NativeIdentity, safe_display_path_text, safe_display_text,
};
use crate::outcome::{OperationOutcome, RunSummary};
use crate::report::{ScanReport, ScanReportState, canonical_scan_report_state};
use crate::scan_coordinator::{
    RelativePath, ScanGeneration, SchedulerSnapshot, SessionCoordinator, WorkCompletion, WorkKind,
    WorkLease, WorkPriority,
};
use crate::scan_store::run_file::SealedRun;
use crate::scan_store::session::ScanStore;
use crate::scan_store::storage::ScanStoreStorage;
use crate::temporary_storage::TemporaryStorage;
use crate::theme::ThemeId;
use crate::ui::palette::ColorCycle;

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const IDLE_INPUT_WAIT: Duration = Duration::from_hours(1);
/// Limits expensive layout rebuilds while a large scan streams in.
const LOADING_FRAME_INTERVAL: Duration = Duration::from_millis(150);
const TRANSIENT_STATUS_DURATION: Duration = Duration::from_millis(250);
const DELETION_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INPUT_BATCH: usize = 32;
/// A scanner event is capped at 32 entries. Keep one owner slice bounded so
/// scanning can never monopolize the UI loop.
const MAX_SCAN_ENTRIES_PER_SLICE: usize = 32;

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct RuntimeSettings {
    pub root: PathBuf,
    pub root_identity: NativeIdentity,
    pub scan_threads: usize,
    pub event_capacity: usize,
    pub cross_filesystems: bool,
    pub exclusions: Vec<String>,
    pub memory_mib: usize,
    pub temporary_storage_mib: usize,
    pub scan_store_mib: Option<usize>,
    pub scan_store_reserve_mib: Option<usize>,
    pub scan_store_dir: Option<PathBuf>,
    pub apparent_size: bool,
    pub disable_delete_confirmation: bool,
    pub reduced_motion: bool,
    pub monochrome: bool,
    pub animate_loading: bool,
    pub theme: ThemeId,
    pub ascii: bool,
    pub mouse: bool,
    pub keymap: KeyPreset,
    pub custom_keys: Option<CustomKeyBindings>,
    pub config_path: Option<PathBuf>,
    pub monochrome_locked: bool,
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

/// Records one rendered deletion counter and reports whether the map changed.
fn deletion_progress_changed(
    previous: &mut Option<(u64, u64)>,
    planned: u64,
    completed: u64,
) -> bool {
    let current = (planned, completed);
    if *previous == Some(current) {
        return false;
    }
    *previous = Some(current);
    true
}

fn deletion_plan_failure_notice(error: &DeletionPlanError) -> &'static str {
    match error {
        DeletionPlanError::Io {
            kind: std::io::ErrorKind::StorageFull,
            ..
        } => {
            "Deletion did not start: temporary storage limit reached; increase --temporary-storage-mib"
        }
        DeletionPlanError::Io {
            kind: std::io::ErrorKind::PermissionDenied,
            ..
        } => "Deletion did not start: access to part of the selected item was denied",
        DeletionPlanError::MemoryLimit { .. } => {
            "Deletion did not start: the selected item exceeds the plan memory limit"
        }
        DeletionPlanError::Io { .. }
        | DeletionPlanError::Synthetic
        | DeletionPlanError::Root
        | DeletionPlanError::InvalidRelativePath
        | DeletionPlanError::Changed
        | DeletionPlanError::Missing(_)
        | DeletionPlanError::Cancelled => {
            "Deletion did not start: the selected item could not be checked"
        }
    }
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
    scan_store_storage: TemporaryStorage,
    summary: RunSummary,
    scan_active: bool,
    /// Latest coalesced session scheduler state; never an event backlog.
    scheduler_snapshot: Option<SchedulerSnapshot>,
    /// The primary breadth-first generation remains active while a versioned refresh may run.
    primary_scan_active: bool,
    /// Scan data relevant to the displayed folder arrived since its last refresh.
    scan_view_dirty: bool,
    /// Folder whose incoming scan changes may refresh the visible map.
    scan_view_root: PathBuf,
    /// Scanner entries queued for one bounded owner scheduling slice.
    pending_scan_entries: VecDeque<ScannedEntry>,
    scan_cancelled: bool,
    generation_rebuild_active: bool,
    /// Root currently owned by the scan rebuild, if any.
    generation_rebuild_target: Option<PathBuf>,
    cancelled_while_scanning: bool,
    exit_after_work: bool,
    timed_actions: Vec<ScheduledAction>,
    next_loading_frame: Duration,
    /// Live mutation progress needs redraws even when accessibility disables animation.
    next_deletion_progress_frame: Duration,
    /// Last rendered counter snapshot; unchanged counters do not redraw the map.
    last_deletion_progress: Option<(u64, u64)>,
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
    let temporary_storage = TemporaryStorage::from_mib(settings.temporary_storage_mib)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let scan_store_session = ScanStoreStorage::new_for_scan_store_mib(
        settings.scan_store_mib,
        settings.scan_store_reserve_mib,
        settings.scan_store_dir.as_deref(),
    )
    .map_err(|error| AppError::Config(error.to_string()))?;
    let scan_store_storage = scan_store_session.quota();
    let mut app = App::new_with_root_identity_and_scan_store(
        terminal_backend,
        settings.root.clone(),
        settings.root_identity.clone(),
        settings.apparent_size,
        settings.disable_delete_confirmation,
        settings.memory_mib,
        settings.keymap,
        settings.custom_keys.clone(),
        settings.mouse,
        scan_store_session,
    )?;
    app.set_loading_animation_enabled(settings.animate_loading);
    let input_runs = app.scan_input_run_factory()?;
    let workers = WorkerPool::start_with_deletion_storage(
        scanner::ScannerOptions {
            session: app.scan_session_id(),
            generation: input_runs.generation(),
            coordinator: app.session_coordinator(),
            root: settings.root.clone(),
            root_identity: Some(settings.root_identity.clone()),
            threads: settings.scan_threads,
            cross_filesystems: settings.cross_filesystems,
            exclusions: settings.exclusions.clone(),
            internal_paths: app.internal_scan_paths(),
            temporary_storage: scan_store_storage.clone(),
            input_runs: Some(input_runs),
        },
        temporary_storage.clone(),
        settings.event_capacity,
    )?;
    let scan_view_root = app.current_folder_path();
    let animation = AnimationScheduler::new(settings.reduced_motion, settings.monochrome, now);
    OwnerLoop {
        app,
        input,
        workers: Some(workers),
        clock,
        animation,
        settings,
        scan_store_storage,
        summary: RunSummary::default(),
        scan_active: true,
        scheduler_snapshot: None,
        primary_scan_active: true,
        scan_view_dirty: false,
        scan_view_root,
        pending_scan_entries: VecDeque::new(),
        scan_cancelled: false,
        generation_rebuild_active: false,
        generation_rebuild_target: None,
        cancelled_while_scanning: false,
        exit_after_work: false,
        timed_actions: Vec::new(),
        next_loading_frame: now.saturating_add(LOADING_FRAME_INTERVAL),
        next_deletion_progress_frame: now.saturating_add(DELETION_PROGRESS_INTERVAL),
        last_deletion_progress: None,
    }
    .run()
}

impl<B> OwnerLoop<B>
where
    B: Backend,
{
    fn run(mut self) -> Result<OperationOutcome<RunSummary>, AppError> {
        let loop_result = self.run_loop();
        let workers = self
            .workers
            .take()
            .ok_or_else(|| AppError::Invariant("worker pool already stopped".to_string()))?;
        let shutdown_result = workers.shutdown();
        let finish_result = self.app.finish();

        let outcome = loop_result?;
        shutdown_result?;
        finish_result?;
        Ok(outcome)
    }

    fn run_loop(&mut self) -> Result<OperationOutcome<RunSummary>, AppError> {
        self.render()?;
        while self.app.is_running {
            let input_processed = self.process_input_batch()?;
            let mut did_work = input_processed;
            // A keystroke must be drawn before background work can spend another
            // scheduling slice. This keeps cursor feedback independent of scan load.
            if input_processed {
                did_work |= self.render()?;
            }
            if !self.app.is_running && self.scan_active {
                self.cancelled_while_scanning = true;
            }
            did_work |= self.process_worker_batch()?;
            did_work |= self.process_deletion_departure(false)?;
            did_work |= self.process_deadlines()?;
            // Scanner batches can keep the loop busy indefinitely. Service a due
            // visual frame before rendering so the map's scan field never waits
            // for the input-poll sleep path to run.
            did_work |= self.render_due_frame()?;

            if !self.app.is_running {
                break;
            }
            if !did_work {
                let input_ready = self.input.poll(self.next_timeout())?;
                if input_ready {
                    self.process_one_input()?;
                }
                // A timeout is work too: while `poll` sleeps, geometry and other
                // deadlines become due. Service them even when no key woke us, or
                // a finished scan leaves the map frozen until the next input.
                self.process_deletion_departure(false)?;
                self.process_deadlines()?;
                self.render_due_frame()?;
            }
        }
        if !self.app.is_running && self.scan_active {
            self.cancelled_while_scanning = true;
        }

        let summary = self.summary.clone();
        let deletion_incomplete = summary
            .deletion_changed_entries
            .saturating_add(summary.deletion_missing_entries)
            .saturating_add(summary.deletion_failed_entries)
            .saturating_add(summary.deletion_unattempted_entries);
        if self.scan_cancelled || self.cancelled_while_scanning {
            Ok(OperationOutcome::Cancelled {
                value: Some(summary),
                precise: true,
            })
        } else if deletion_incomplete > 0 {
            Ok(OperationOutcome::Partial {
                completed_entries: summary.deleted_entries,
                failed_entries: deletion_incomplete,
                value: summary,
            })
        } else if self.app.scan_is_uncertain(&summary) {
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
        let result = match self.input.read()? {
            InputEvent::Barrier => {
                self.animation.set_activity_suspended(true);
                let result = (|| {
                    self.render()?;
                    self.wait_for_quiescence()
                })();
                self.animation.set_activity_suspended(false);
                result.map(|()| true)
            }
            InputEvent::Terminal(Event::Resize(_, _)) => {
                self.app.reset_ui_mode();
                self.app.show_next_deletion_confirmation();
                self.app.mark_dirty();
                Ok(false)
            }
            InputEvent::Terminal(event) => {
                let command = handle_keypress(&event, &mut self.app);
                let is_drill = matches!(command, InputCommand::Drill);
                self.handle_input_command(command).map(|()| is_drill)
            }
        };
        self.flush_deletion_plan_cancellation()?;
        result
    }

    fn flush_deletion_plan_cancellation(&mut self) -> Result<(), AppError> {
        if self.app.take_deletion_plan_cancellation() {
            self.workers()?.cancel_deletion_plan();
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_input_command(&mut self, command: InputCommand) -> Result<(), AppError> {
        let now = self.clock.now();
        let drilled = matches!(command, InputCommand::Drill);
        match command {
            InputCommand::Drill | InputCommand::Navigation => {
                if self.app.ui_mode.allows_motion() {
                    self.app.mark_dirty();
                }
            }
            InputCommand::None => {}
            InputCommand::PathError => {
                self.app.set_path_to_red();
                self.schedule(now, TimedAction::ResetPathColor, TRANSIENT_STATUS_DURATION);
            }
            InputCommand::CancelGenerationRebuild => {
                if self.generation_rebuild_active {
                    self.workers()?.cancel_generation_rebuild();
                }
            }
            InputCommand::RequestDeletion(target) => {
                let reduced_guardrails = self.app.reduced_deletion_guardrails();
                let maximum_bytes = self.app.maximum_deletion_plan_bytes();
                if self.app.queue_deletion_confirmation(
                    *target,
                    reduced_guardrails,
                    maximum_bytes,
                    now,
                ) {
                    self.app.show_next_deletion_confirmation();
                }
            }
            InputCommand::CancelDeletionConfirmation => {
                self.app.cancel_deletion_confirmation();
            }
            InputCommand::ConfirmDeletion { work_id, target } => {
                let target_path = target.full_path();
                let overlaps_scan_rebuild =
                    self.generation_rebuild_target.as_ref().is_some_and(|root| {
                        target_path.starts_with(root) || root.starts_with(&target_path)
                    });
                if self.app.queue_confirmed_deletion(work_id, target, now) {
                    if overlaps_scan_rebuild {
                        self.workers()?.cancel_generation_rebuild();
                    }
                    self.start_next_deletion_planning()?;
                }
            }
            InputCommand::ExportScan => {
                let result = next_export_path("scan-report").and_then(|path| {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map_err(|error| error.to_string())?;
                    self.app
                        .write_scan_report(&self.summary, &mut file)
                        .map_err(|error| error.to_string())?;
                    Ok(path)
                });
                match result {
                    Ok(path) => self
                        .app
                        .show_notice(export_notice("Scan report exported to", &path)),
                    Err(error) => self.app.show_error(format!("Scan export failed: {error}")),
                }
            }
            InputCommand::ExportDeletionHistory => {
                let result = next_export_path("deletion-history").and_then(|path| {
                    let mut file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map_err(|error| error.to_string())?;
                    self.app
                        .write_deletion_history(&mut file)
                        .map_err(|error| error.to_string())?;
                    Ok(path)
                });
                match result {
                    Ok(path) => {
                        self.app.clear_deletion_history();
                        self.app
                            .show_notice(export_notice("Deletion history exported to", &path));
                    }
                    Err(error) => self
                        .app
                        .show_error(format!("Deletion history export failed: {error}")),
                }
            }
            InputCommand::OpenThemePicker => self.app.open_theme_picker(self.settings.theme),
            InputCommand::PreviewTheme(theme) | InputCommand::RestoreTheme(theme) => {
                self.set_theme(theme);
            }
            InputCommand::CommitTheme { original, selected } => {
                self.set_theme(selected);
                if selected != original
                    && let Err(error) = self.persist_theme_selection(selected)
                {
                    self.app.show_error(format!(
                        "Theme applied for this session, but could not be saved: {error}"
                    ));
                }
            }
            InputCommand::PromptExit => {
                self.process_deletion_departure(true)?;
                self.app.prompt_exit(self.exit_work());
            }
            InputCommand::CancelPendingWorkAndExit => self.cancel_pending_work_and_exit(false)?,
            InputCommand::StopDeletionAndExit => self.cancel_pending_work_and_exit(true)?,
        }
        if drilled {
            self.scan_view_root = self.app.current_folder_path();
            if self.primary_scan_active {
                self.workers()?.prioritize_scan(&self.scan_view_root);
            }
        }
        if !self.exit_after_work {
            self.start_next_deletion_planning()?;
            self.start_next_deletion_execution()?;
        }
        self.finish_exit_after_work();
        self.flush_deletion_plan_cancellation()?;
        if !self.app.ui_mode.allows_motion() {
            self.animation.cancel_all();
        }
        Ok(())
    }

    fn set_theme(&mut self, theme: ThemeId) {
        self.settings.theme = theme;
        self.settings.monochrome = self.settings.monochrome_locked || theme == ThemeId::Monochrome;
        self.animation
            .set_accessibility(self.settings.reduced_motion, self.settings.monochrome);
        self.app.mark_dirty();
    }

    fn persist_theme_selection(&self, theme: ThemeId) -> Result<(), AppError> {
        let path =
            self.settings.config_path.as_deref().ok_or_else(|| {
                AppError::Config("no writable config path is available".to_string())
            })?;
        save_theme_preference(path, theme)
    }

    fn exit_work(&self) -> ExitWork {
        if let Some((planned_entries, completed)) = self.app.deletion_work.active_progress() {
            return ExitWork::Active {
                planned_entries,
                completed,
                pending: self.app.deletion_work.pending_count(),
            };
        }
        let pending = self.app.deletion_work.pending_count();
        if pending > 0 {
            ExitWork::Pending { count: pending }
        } else {
            ExitWork::None
        }
    }

    fn cancel_pending_work_and_exit(&mut self, stop_active: bool) -> Result<(), AppError> {
        self.process_deletion_departure(true)?;
        self.app.cancel_pending_deletion_work();
        self.flush_deletion_plan_cancellation()?;
        self.exit_after_work = true;
        let work = if stop_active {
            if let Some((planned_entries, completed)) = self.app.deletion_work.active_progress() {
                self.workers()?.safely_stop_deletion();
                ExitWork::Stopping {
                    planned_entries,
                    completed,
                }
            } else {
                ExitWork::Cancelling {
                    count: self.app.deletion_work.pending_count(),
                }
            }
        } else {
            ExitWork::Cancelling {
                count: self.app.deletion_work.pending_count(),
            }
        };
        self.app.prompt_exit(work);
        Ok(())
    }

    /// Rebuilds an invalidated canonical generation once no scan scope owns
    /// the scanner. Primary scan outputs remain isolated from this new factory.
    fn start_pending_generation_rebuild(&mut self) -> Result<bool, AppError> {
        if self.primary_scan_active || self.generation_rebuild_active || self.workers.is_none() {
            return Ok(false);
        }
        if !self.app.begin_generation_rebuild()? {
            return Ok(false);
        }
        let input_runs = self.app.scan_input_run_factory()?;
        let root = self.settings.root.clone();
        let options = scanner::ScannerOptions {
            session: self.app.scan_session_id(),
            generation: input_runs.generation(),
            coordinator: self.app.session_coordinator(),
            root: root.clone(),
            root_identity: Some(self.settings.root_identity.clone()),
            threads: self.settings.scan_threads,
            cross_filesystems: self.settings.cross_filesystems,
            exclusions: self.settings.exclusions.clone(),
            internal_paths: self.app.internal_scan_paths(),
            temporary_storage: self.scan_store_storage.clone(),
            input_runs: Some(input_runs),
        };
        if let Err(error) = self.workers()?.request_generation_rebuild(options) {
            self.app.cancel_generation_rebuild()?;
            return Err(error);
        }
        self.scan_view_root.clone_from(&root);
        self.scan_active = true;
        self.generation_rebuild_active = true;
        self.generation_rebuild_target = Some(root);
        self.next_loading_frame = self.clock.now().saturating_add(LOADING_FRAME_INTERVAL);
        Ok(true)
    }

    fn start_next_deletion_planning(&mut self) -> Result<(), AppError> {
        let Some(command) = self.app.next_deletion_planning_work() else {
            return Ok(());
        };
        self.submit_deletion_work(command)
    }

    fn start_next_deletion_execution(&mut self) -> Result<(), AppError> {
        if self.app.has_deletion_departure() {
            return Ok(());
        }
        let Some(command) = self.app.next_deletion_execution_work() else {
            return Ok(());
        };
        self.submit_deletion_work(command)
    }

    fn submit_deletion_work(
        &mut self,
        command: crate::state::deletion_work::DeletionWorkCommand,
    ) -> Result<(), AppError> {
        match self.workers()?.submit_deletion_work(command) {
            Ok(()) => {
                self.app.mark_dirty();
                Ok(())
            }
            Err(DeletionWorkSubmissionError::Busy(command)) => {
                self.app.restore_deletion_work(*command);
                Err(AppError::Invariant(
                    "deletion worker lane was unexpectedly occupied".to_string(),
                ))
            }
            Err(DeletionWorkSubmissionError::Disconnected) => {
                Err(AppError::Worker("deletion worker disconnected".to_string()))
            }
        }
    }

    fn finish_exit_after_work(&mut self) {
        if !self.exit_after_work
            || self.generation_rebuild_active
            || self.app.deletion_work.has_work()
            || self.app.has_deletion_departure()
        {
            return;
        }
        self.exit_after_work = false;
        self.app.exit();
    }

    fn refresh_scheduler_snapshot(&mut self) {
        let snapshot = self
            .workers
            .as_ref()
            .and_then(WorkerPool::scheduler_snapshot);
        if snapshot != self.scheduler_snapshot {
            self.scheduler_snapshot = snapshot;
            self.app.set_scheduler_snapshot(snapshot);
        }
    }

    fn process_worker_batch(&mut self) -> Result<bool, AppError> {
        self.refresh_scheduler_snapshot();
        if self.app.map_is_transitioning() || self.input.poll(Duration::ZERO)? {
            return Ok(false);
        }
        if self.process_pending_scan_entries()? {
            return Ok(true);
        }
        let event = match self.workers()?.events().try_recv() {
            Ok(event) => event,
            Err(TryRecvError::Empty) => return Ok(false),
            Err(TryRecvError::Disconnected) => {
                if self.scan_active || self.app.deletion_work.has_background_activity() {
                    return Err(AppError::Worker(
                        "worker event channel disconnected".to_string(),
                    ));
                }
                return Ok(false);
            }
        };
        self.handle_worker_event(event)?;
        self.flush_deletion_plan_cancellation()?;
        Ok(true)
    }

    fn process_pending_scan_entries(&mut self) -> Result<bool, AppError> {
        let mut processed = false;
        for _ in 0..MAX_SCAN_ENTRIES_PER_SLICE {
            if !self.process_pending_scan_entry() {
                break;
            }
            processed = true;
            // Each model mutation is a cooperative yield point. A full scan queue
            // must not defer an already due visual frame until its 32-entry slice ends.
            if self.input.poll(Duration::ZERO)? {
                break;
            }
            if self
                .animation
                .next_frame_at()
                .is_some_and(|deadline| self.clock.now() >= deadline)
            {
                self.render_due_frame()?;
            }
        }
        Ok(processed)
    }

    fn process_pending_scan_entry(&mut self) -> bool {
        let Some(entry) = self.pending_scan_entries.pop_front() else {
            return false;
        };
        self.handle_scan_entry(entry);
        true
    }

    fn handle_scan_entry(&mut self, entry: ScannedEntry) {
        self.scan_view_dirty |= entry.path.starts_with(&self.scan_view_root);
        if self.primary_scan_active {
            self.summary.scanned_entries = self.summary.scanned_entries.saturating_add(1);
        }
        self.app.record_loading_entry(entry.path);
    }

    fn admit_coverage_runs(
        &mut self,
        lease: Option<&WorkLease>,
        input_runs: Vec<SealedRun>,
    ) -> Result<(), AppError> {
        if input_runs.is_empty() {
            self.app.record_scan_store_unrecorded_path();
            return Ok(());
        }
        let lease = lease.ok_or_else(|| {
            AppError::Invariant("scanner emitted an unleased sealed coverage result".to_string())
        })?;
        self.app.admit_scan_input_runs(lease, input_runs);
        Ok(())
    }

    fn handle_primary_unscanned(
        &mut self,
        lease: Option<&WorkLease>,
        input_runs: Vec<SealedRun>,
        path: &Path,
        reason: &crate::model::UnscannedReason,
    ) -> Result<(), AppError> {
        self.admit_coverage_runs(lease, input_runs)?;
        self.scan_view_dirty |= path.starts_with(&self.scan_view_root);
        if self.generation_rebuild_active {
            return Ok(());
        }
        self.summary.unscanned_entries = self.summary.unscanned_entries.saturating_add(1);
        self.summary.last_unscanned_path = Some(safe_display_path_text(path));
        self.summary.last_unscanned_reason = Some(display_reason(reason));
        match reason {
            crate::model::UnscannedReason::Excluded(_) => {
                self.summary.excluded_entries = self.summary.excluded_entries.saturating_add(1);
            }
            crate::model::UnscannedReason::FilesystemBoundary => {
                self.summary.filesystem_boundaries =
                    self.summary.filesystem_boundaries.saturating_add(1);
            }
            crate::model::UnscannedReason::SymbolicLink => {
                self.summary.link_entries = self.summary.link_entries.saturating_add(1);
            }
            crate::model::UnscannedReason::Metadata(message)
            | crate::model::UnscannedReason::Replacement(message) => {
                self.summary.unreadable_entries = self.summary.unreadable_entries.saturating_add(1);
                self.summary.last_unreadable_path = self.summary.last_unscanned_path.clone();
                self.summary.last_worker_error = Some(safe_display_text(message));
                self.app.increment_failed_to_read();
            }
            crate::model::UnscannedReason::IdentityStorageCapacity => {}
        }
        Ok(())
    }

    fn handle_scan_failure(&mut self, path: Option<&Path>, message: &str, rebuild_active: bool) {
        if !rebuild_active {
            self.scan_view_dirty |= path.is_some_and(|path| path.starts_with(&self.scan_view_root));
        }
        self.app.record_scan_store_unrecorded_path();
        let message = safe_display_text(message);
        self.summary.unscanned_entries = self.summary.unscanned_entries.saturating_add(1);
        self.summary.unreadable_entries = self.summary.unreadable_entries.saturating_add(1);
        self.summary.last_unscanned_path = path.map(safe_display_path_text);
        self.summary.last_unreadable_path = self.summary.last_unscanned_path.clone();
        self.summary.last_unscanned_reason = Some(message.clone());
        self.summary.last_worker_error = Some(message);
        if !rebuild_active {
            self.app.increment_failed_to_read();
        }
        self.animation.schedule_error();
    }

    fn finish_primary_scan(&mut self, cancelled: bool) -> Result<(), AppError> {
        self.primary_scan_active = false;
        self.scan_view_dirty = false;
        if !self.generation_rebuild_active {
            self.scan_active = false;
        }
        self.scan_cancelled = cancelled;
        if cancelled {
            self.app.cancel_primary_scan()?;
        } else {
            let reduction = self.workers()?.acquire_reducer()?;
            self.app.finalize_scan();
            self.workers()?
                .finish_coordinated_work(reduction, WorkCompletion::Succeeded)?;
            self.app.start_ui();
            self.animation.schedule_completion();
        }
        let (used, limit) = self.app.scan_store_stats();
        self.summary.scan_store_bytes = used;
        self.summary.scan_store_limit_bytes = limit;
        self.start_pending_generation_rebuild()?;
        Ok(())
    }
    fn finish_generation_rebuild(&mut self, cancelled: bool) -> Result<(), AppError> {
        self.generation_rebuild_active = false;
        self.generation_rebuild_target = None;
        if let Some(workers) = self.workers.as_ref() {
            workers.finish_generation_rebuild(if cancelled {
                WorkCompletion::Cancelled
            } else {
                WorkCompletion::Succeeded
            })?;
        }
        if !self.primary_scan_active {
            self.scan_active = false;
        }
        if cancelled {
            self.app.cancel_generation_rebuild()?;
        } else {
            self.app.finish_generation_rebuild()?;
        }
        let (used, limit) = self.app.scan_store_stats();
        self.summary.scan_store_bytes = used;
        self.summary.scan_store_limit_bytes = limit;
        self.start_pending_generation_rebuild()?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_worker_event(&mut self, event: WorkerEvent) -> Result<(), AppError> {
        let exit_work_may_change = matches!(
            &event,
            WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. }
                | WorkerEvent::ScanFinished { .. }
        );
        match event {
            WorkerEvent::ScanBatch {
                lease,
                entries,
                input_runs,
            } => {
                if !input_runs.is_empty() {
                    let lease = lease.as_ref().ok_or_else(|| {
                        AppError::Invariant("scanner emitted an unleased sealed batch".to_string())
                    })?;
                    self.app.admit_scan_input_runs(lease, input_runs);
                }
                debug_assert!(self.pending_scan_entries.is_empty());
                self.pending_scan_entries.extend(entries);
            }
            WorkerEvent::ScanUnscanned {
                lease,
                path,
                reason,
                input_runs,
            } => {
                self.handle_primary_unscanned(lease.as_ref(), input_runs, &path, &reason)?;
            }
            WorkerEvent::ScanFailed { path, message } => {
                self.handle_scan_failure(path.as_deref(), &message, self.generation_rebuild_active);
            }
            WorkerEvent::ScanFinished { cancelled } => {
                if self.generation_rebuild_active {
                    self.finish_generation_rebuild(cancelled)?;
                } else {
                    self.finish_primary_scan(cancelled)?;
                }
            }
            WorkerEvent::DeletionPlanned { work_id, result } => match result {
                Ok(plan) => {
                    if self.app.deletion_plan_ready(work_id, plan) {
                        self.app.mark_dirty();
                    }
                }
                Err(error) if error.is_cancelled() => {
                    self.app.deletion_plan_cancelled(work_id);
                    self.app.mark_dirty();
                }
                Err(error) if error.is_stale() || error.is_missing() => {
                    if self.app.deletion_plan_stale(work_id) {
                        if error.is_missing() {
                            self.summary.deletion_missing_entries =
                                self.summary.deletion_missing_entries.saturating_add(1);
                        } else {
                            self.summary.deletion_changed_entries =
                                self.summary.deletion_changed_entries.saturating_add(1);
                        }
                        self.app.record_deletion_notice(
                            "Deletion did not start: the selected item changed or disappeared",
                        );
                    }
                }
                Err(error) => {
                    let notice = deletion_plan_failure_notice(&error);
                    if self.app.deletion_plan_failed(work_id) {
                        self.summary.deletion_failed_entries =
                            self.summary.deletion_failed_entries.saturating_add(1);
                        self.app.record_deletion_notice(notice);
                    }
                }
            },
            WorkerEvent::DeletionExecutionRejected { work_id, error } => {
                if error.is_cancelled() {
                    self.app
                        .deletion_execution_finished(work_id, WorkCompletion::Cancelled);
                    self.app.mark_dirty();
                } else {
                    let invalidated = error.is_missing() || error.is_stale();
                    let notice = if invalidated {
                        "Deletion skipped: files changed or disappeared"
                    } else {
                        "Deletion stopped: a final safety check failed"
                    };
                    let resolved = if invalidated {
                        self.app.deletion_execution_stale(work_id)
                    } else {
                        self.app.deletion_execution_failed(work_id)
                    };
                    if resolved {
                        if error.is_missing() {
                            self.summary.deletion_missing_entries =
                                self.summary.deletion_missing_entries.saturating_add(1);
                        } else if error.is_stale() {
                            self.summary.deletion_changed_entries =
                                self.summary.deletion_changed_entries.saturating_add(1);
                        } else {
                            self.summary.deletion_failed_entries =
                                self.summary.deletion_failed_entries.saturating_add(1);
                        }
                        self.app.record_deletion_notice(notice);
                    }
                }
            }
            WorkerEvent::DeletionFinished { work_id, report } => {
                let completion = if report.soft_cancelled {
                    WorkCompletion::Cancelled
                } else {
                    WorkCompletion::Succeeded
                };
                if self.app.deletion_execution_finished(work_id, completion) {
                    self.summary.deleted_entries = self
                        .summary
                        .deleted_entries
                        .saturating_add(report.deleted_entries());
                    self.summary.deletion_changed_entries = self
                        .summary
                        .deletion_changed_entries
                        .saturating_add(report.changed_entries());
                    self.summary.deletion_missing_entries = self
                        .summary
                        .deletion_missing_entries
                        .saturating_add(report.missing_entries());
                    self.summary.deletion_failed_entries = self
                        .summary
                        .deletion_failed_entries
                        .saturating_add(report.failed_entries());
                    self.summary.deletion_unattempted_entries = self
                        .summary
                        .deletion_unattempted_entries
                        .saturating_add(report.unattempted_entries());
                    if self.animation.animations_enabled()
                        && !self.settings.ascii
                        && ColorCycle::can_animate(
                            crate::theme::Theme::for_id(self.settings.theme).focus,
                        )
                        && report.target_was_removed()
                    {
                        let _ = self.app.begin_deletion_departure(
                            &report.scan_root,
                            &report.root_relative_path,
                            report.deleted_entries(),
                            self.clock.now(),
                        );
                    }
                    self.reconcile_deletion_report(report)?;
                }
            }
        }
        if exit_work_may_change
            && !self.exit_after_work
            && matches!(self.app.ui_mode, crate::UiMode::Exiting { .. })
        {
            self.app.prompt_exit(self.exit_work());
        }
        if !self.exit_after_work {
            self.start_next_deletion_planning()?;
            self.start_next_deletion_execution()?;
        }
        self.finish_exit_after_work();
        Ok(())
    }

    fn reconcile_deletion_report(&mut self, report: DeletionReport) -> Result<(), AppError> {
        if self.app.complete_deletion(report) {
            self.animation.schedule_deletion_result();
            self.app.flash_space_freed();
            self.schedule(
                self.clock.now(),
                TimedAction::UnflashSpace,
                TRANSIENT_STATUS_DURATION,
            );
        } else {
            self.animation.schedule_deletion_result();
        }
        self.start_pending_generation_rebuild()?;
        Ok(())
    }

    /// Clears a copied departure after it finishes dissolving during the reflow.
    fn process_deletion_departure(&mut self, force: bool) -> Result<bool, AppError> {
        if !self.app.has_deletion_departure() {
            return Ok(false);
        }
        if !force && !self.app.deletion_departure_is_finished(self.clock.now()) {
            return Ok(false);
        }
        self.app.clear_deletion_departure();
        if !self.exit_after_work {
            self.start_next_deletion_planning()?;
            self.start_next_deletion_execution()?;
        }
        self.finish_exit_after_work();
        Ok(true)
    }

    fn process_deadlines(&mut self) -> Result<bool, AppError> {
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

        if self.scan_active && now >= self.next_loading_frame {
            if self.scan_view_dirty && self.app.refresh_board_from_scan()? {
                self.scan_view_dirty = false;
            }
            self.next_loading_frame = now.saturating_add(LOADING_FRAME_INTERVAL);
            processed = true;
        }
        if let Some((planned, progress)) = self.app.deletion_work.active_progress() {
            if now >= self.next_deletion_progress_frame {
                if deletion_progress_changed(
                    &mut self.last_deletion_progress,
                    planned,
                    progress.load(std::sync::atomic::Ordering::Acquire),
                ) {
                    self.app.mark_dirty();
                    processed = true;
                }
                self.next_deletion_progress_frame = now.saturating_add(DELETION_PROGRESS_INTERVAL);
            }
        } else {
            self.last_deletion_progress = None;
            self.next_deletion_progress_frame = now.saturating_add(DELETION_PROGRESS_INTERVAL);
        }
        Ok(processed)
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

    /// Renders a scheduled visual frame whether or not scan work kept this loop busy.
    fn render_due_frame(&mut self) -> Result<bool, AppError> {
        self.update_animation_frame();
        self.render()
    }

    fn render(&mut self) -> Result<bool, AppError> {
        let result = self.app.render_if_dirty(
            &mut self.animation,
            self.clock.now(),
            self.settings.theme.attribution().name,
            crate::theme::Theme::for_id(self.settings.theme),
            self.settings.ascii,
            self.settings.monochrome,
            self.settings.reduced_motion,
        );
        if matches!(&result, Ok(true)) {
            crate::app::emit_pty_test_marker("TERMINAL_READY");
        }

        self.flush_deletion_plan_cancellation()?;
        result
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
        let mut timeout = if self.scan_active || self.app.deletion_work.has_background_activity() {
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
        if let Some(deadline) = self.app.deletion_departure_deadline() {
            timeout = timeout.min(deadline.saturating_sub(now));
        }
        if self.app.deletion_work.has_active_mutation() {
            timeout = timeout.min(self.next_deletion_progress_frame.saturating_sub(now));
        }
        if let Some(deadline) = self.timed_actions.iter().map(|action| action.at).min() {
            timeout = timeout.min(deadline.saturating_sub(now));
        }
        timeout
    }

    fn wait_for_quiescence(&mut self) -> Result<(), AppError> {
        'quiescence: loop {
            while self.scan_active || self.app.deletion_work.has_background_activity() {
                if self.process_pending_scan_entry() {
                    self.render_due_frame()?;
                    continue;
                }
                match self.workers()?.events().recv_timeout(WORKER_POLL_INTERVAL) {
                    Ok(event) => {
                        self.handle_worker_event(event)?;
                        self.render_due_frame()?;
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
                if self.process_deletion_departure(false)? {
                    self.render_due_frame()?;
                }
                if self.scan_active || self.app.deletion_work.has_background_activity() {
                    continue 'quiescence;
                }
                let next_timed = self.timed_actions.iter().map(|action| action.at).min();
                let next_animation = self.animation.next_frame_at();
                let next_departure = self.app.deletion_departure_deadline();
                let next = next_timed
                    .into_iter()
                    .chain(next_animation)
                    .chain(next_departure)
                    .min();
                let Some(next) = next else {
                    break;
                };
                if !self.clock.advance_to(next) {
                    break;
                }
                self.process_deletion_departure(false)?;
                self.process_deadlines()?;
                self.render_due_frame()?;
            }
            break;
        }
        Ok(())
    }

    fn workers(&self) -> Result<&WorkerPool, AppError> {
        self.workers
            .as_ref()
            .ok_or_else(|| AppError::Invariant("worker pool unavailable".to_string()))
    }
}

/// Internal scanner probe used only by the Criterion benchmark harness.
#[cfg(feature = "internal")]
pub(crate) struct BenchmarkScan {
    options: scanner::ScannerOptions,
    workers: Option<WorkerPool>,
}

#[cfg(feature = "internal")]
impl BenchmarkScan {
    const EVENT_CAPACITY: usize = 4_096;
    const COMPLETION_TIMEOUT: Duration = Duration::from_secs(30);

    pub(crate) fn start(root: &Path, threads: usize) -> Result<Self, AppError> {
        let session = crate::scan_session::ScanSessionId::random().map_err(|error| {
            AppError::Worker(format!("could not create benchmark scan session: {error}"))
        })?;
        let coordinator = SessionCoordinator::start(session, ScanGeneration::initial())
            .map_err(|error| AppError::Worker(error.to_string()))?;
        let options = scanner::ScannerOptions {
            session,
            generation: ScanGeneration::initial(),
            coordinator,
            root: root.to_path_buf(),
            root_identity: None,
            threads,
            cross_filesystems: false,
            exclusions: Vec::new(),
            internal_paths: Vec::new(),
            temporary_storage: TemporaryStorage::default(),
            input_runs: None,
        };
        let workers = WorkerPool::start_with_deletion_storage(
            options.clone(),
            TemporaryStorage::default(),
            Self::EVENT_CAPACITY,
        )?;
        Ok(Self {
            options,
            workers: Some(workers),
        })
    }

    pub(crate) fn scan_to_completion(root: &Path, threads: usize) -> Result<usize, AppError> {
        let mut scan = Self::start(root, threads)?;
        let result = scan.await_scan_finished(false);
        scan.shutdown()?;
        result
    }

    pub(crate) fn complete_initial_scan(&self) -> Result<usize, AppError> {
        self.await_scan_finished(false)
    }

    pub(crate) fn start_focus_scan(&self) -> Result<(), AppError> {
        self.workers()?
            .request_generation_rebuild(self.options.clone())
    }

    pub(crate) fn focus_latency(&self, paths: &[PathBuf]) -> Result<Duration, AppError> {
        let workers = self.workers()?;
        let snapshot = self.await_active_scan()?;
        let initial_epoch = snapshot.focus_epoch();
        let started = Instant::now();
        let required = u64::try_from(paths.len()).unwrap_or(u64::MAX);
        loop {
            for path in paths {
                workers.prioritize_scan(path);
            }
            if workers.scheduler_snapshot().is_some_and(|snapshot| {
                snapshot.focus_epoch().wrapping_sub(initial_epoch) >= required
            }) {
                return Ok(started.elapsed());
            }
            if started.elapsed() >= Self::COMPLETION_TIMEOUT {
                return Err(AppError::Worker(
                    "scanner benchmark did not process all focus requests".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    pub(crate) fn cancel_rebuild_latency(&self) -> Result<Duration, AppError> {
        let workers = self.workers()?;
        workers.request_pre_cancelled_generation_rebuild(self.options.clone())?;
        let started = Instant::now();
        let result = self.await_scan_finished(true).map(|_| started.elapsed());
        workers.finish_generation_rebuild(WorkCompletion::Cancelled)?;
        result
    }

    fn await_active_scan(&self) -> Result<SchedulerSnapshot, AppError> {
        let workers = self.workers()?;
        let waiting_started = Instant::now();
        loop {
            if let Some(snapshot) = workers.scheduler_snapshot()
                && (snapshot.active_leases() > 0 || snapshot.pending().total() > 0)
            {
                return Ok(snapshot);
            }
            if waiting_started.elapsed() >= Self::COMPLETION_TIMEOUT {
                return Err(AppError::Worker(
                    "scanner benchmark did not begin scan work".to_string(),
                ));
            }
            std::thread::sleep(Duration::from_micros(50));
        }
    }

    fn workers(&self) -> Result<&WorkerPool, AppError> {
        self.workers
            .as_ref()
            .ok_or_else(|| AppError::Invariant("scanner benchmark has already stopped".to_string()))
    }

    fn await_scan_finished(&self, expected_cancelled: bool) -> Result<usize, AppError> {
        let workers = self.workers()?;
        let mut entries = 0_usize;
        loop {
            match workers.events().recv_timeout(Self::COMPLETION_TIMEOUT) {
                Ok(WorkerEvent::ScanBatch { entries: batch, .. }) => {
                    entries = entries.saturating_add(batch.len());
                }
                Ok(WorkerEvent::ScanFinished { cancelled }) if cancelled == expected_cancelled => {
                    return Ok(entries);
                }
                Ok(WorkerEvent::ScanFinished { cancelled }) => {
                    return Err(AppError::Worker(format!(
                        "scanner benchmark completed with cancelled={cancelled}, expected {expected_cancelled}"
                    )));
                }
                Ok(WorkerEvent::ScanFailed { message, .. }) => {
                    return Err(AppError::Worker(format!(
                        "scanner benchmark failed: {message}"
                    )));
                }
                Ok(
                    WorkerEvent::ScanUnscanned { .. }
                    | WorkerEvent::DeletionPlanned { .. }
                    | WorkerEvent::DeletionExecutionRejected { .. }
                    | WorkerEvent::DeletionFinished { .. },
                ) => {}
                Err(RecvTimeoutError::Timeout) => {
                    return Err(AppError::Worker(
                        "scanner benchmark timed out waiting for completion".to_string(),
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(AppError::Worker(
                        "scanner benchmark worker disconnected".to_string(),
                    ));
                }
            }
        }
    }

    fn shutdown(&mut self) -> Result<(), AppError> {
        self.workers.take().map_or(Ok(()), WorkerPool::shutdown)
    }
}

#[cfg(feature = "internal")]
impl Drop for BenchmarkScan {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}
fn export_notice(prefix: &str, path: &Path) -> String {
    format!("{prefix} {}", safe_display_path_text(path))
}

fn display_reason(reason: &crate::model::UnscannedReason) -> String {
    let deceptive = match reason {
        crate::model::UnscannedReason::Excluded(value)
        | crate::model::UnscannedReason::Metadata(value)
        | crate::model::UnscannedReason::Replacement(value) => {
            safe_display_text(value).starts_with(DECEPTIVE_DISPLAY_MARKER)
        }
        crate::model::UnscannedReason::SymbolicLink
        | crate::model::UnscannedReason::FilesystemBoundary
        | crate::model::UnscannedReason::IdentityStorageCapacity => false,
    };
    let rendered = safe_display_text(&format!("{reason:?}"));
    if deceptive && !rendered.contains(DECEPTIVE_DISPLAY_MARKER) {
        format!("{DECEPTIVE_DISPLAY_MARKER} {rendered}")
    } else {
        rendered
    }
}

fn is_scan_store_capacity_failure(message: &str) -> bool {
    message.contains("scan store capacity exhausted") && message.contains("--scan-store-mib")
}

fn summary_only_scan_outcome(
    root: PathBuf,
    root_identity: NativeIdentity,
    summary: RunSummary,
) -> OperationOutcome<ScanReport> {
    OperationOutcome::Partial {
        completed_entries: summary.scanned_entries,
        failed_entries: summary.unscanned_entries,
        value: ScanReport::summary_only(root, Some(root_identity), summary),
    }
}

/// Runs the production scanner and canonical store without acquiring a terminal.
///
/// # Errors
/// Returns a scanner, model, or worker error after all owned workers stop.
#[allow(clippy::needless_pass_by_value)]
pub fn scan_headless(settings: RuntimeSettings) -> Result<OperationOutcome<ScanReport>, AppError> {
    let scan_store_session = ScanStoreStorage::new_for_scan_store_mib(
        settings.scan_store_mib,
        settings.scan_store_reserve_mib,
        settings.scan_store_dir.as_deref(),
    )
    .map_err(|error| AppError::Config(error.to_string()))?;
    scan_headless_with_scan_store_session(settings, scan_store_session)
}

#[cfg(test)]
fn scan_headless_with_scan_store_storage(
    settings: RuntimeSettings,
    scan_store_storage: TemporaryStorage,
) -> Result<OperationOutcome<ScanReport>, AppError> {
    let scan_store_session =
        ScanStoreStorage::new(scan_store_storage, settings.scan_store_dir.as_deref())
            .map_err(|error| AppError::Config(error.to_string()))?;
    scan_headless_with_scan_store_session(settings, scan_store_session)
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn scan_headless_with_scan_store_session(
    settings: RuntimeSettings,
    scan_store_session: ScanStoreStorage,
) -> Result<OperationOutcome<ScanReport>, AppError> {
    let scan_store_storage = scan_store_session.quota();
    let temporary_storage = TemporaryStorage::from_mib(settings.temporary_storage_mib)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let mut scan_store = ScanStore::new_with_storage(ScanGeneration::initial(), scan_store_session)
        .map_err(|error| AppError::Model(error.to_string()))?;
    let input_runs = scan_store
        .input_run_factory()
        .map_err(|error| AppError::Model(error.to_string()))?;
    let coordinator = SessionCoordinator::start(scan_store.session(), input_runs.generation())
        .map_err(|error| AppError::Worker(error.to_string()))?;
    let workers = WorkerPool::start_with_deletion_storage(
        scanner::ScannerOptions {
            session: scan_store.session(),
            generation: input_runs.generation(),
            coordinator: coordinator.clone(),
            root: settings.root.clone(),
            root_identity: Some(settings.root_identity.clone()),
            threads: settings.scan_threads,
            cross_filesystems: settings.cross_filesystems,
            exclusions: settings.exclusions.clone(),
            internal_paths: Vec::new(),
            temporary_storage: scan_store_storage,
            input_runs: Some(input_runs),
        },
        temporary_storage,
        settings.event_capacity,
    )?;
    let mut summary = RunSummary::default();
    let mut scan_store_capacity_exhausted = false;
    let scan_result = (|| -> Result<bool, AppError> {
        loop {
            let event = workers
                .events()
                .recv()
                .map_err(|_| AppError::Worker("scanner event channel disconnected".to_string()))?;
            match event {
                WorkerEvent::ScanBatch {
                    lease,
                    entries,
                    input_runs,
                } => {
                    if !scan_store_capacity_exhausted && !input_runs.is_empty() {
                        let lease = lease.as_ref().ok_or_else(|| {
                            AppError::Invariant(
                                "scanner emitted an unleased sealed batch".to_string(),
                            )
                        })?;
                        for run in input_runs {
                            if let Err(error) = scan_store.accept_leased_input_run(lease, run) {
                                let message = error.to_string();
                                if !is_scan_store_capacity_failure(&message) {
                                    return Err(AppError::Model(message));
                                }
                                scan_store_capacity_exhausted = true;
                                scan_store
                                    .discard_active()
                                    .map_err(|error| AppError::Model(error.to_string()))?;
                                summary.unscanned_entries =
                                    summary.unscanned_entries.saturating_add(1);
                                summary.last_unscanned_reason = Some(message.clone());
                                summary.last_worker_error = Some(message);
                                break;
                            }
                        }
                    }
                    summary.scanned_entries =
                        summary.scanned_entries.saturating_add(entries.len() as u64);
                }
                WorkerEvent::ScanUnscanned {
                    lease,
                    path,
                    reason,
                    input_runs,
                } => {
                    let represented = !input_runs.is_empty();
                    if !scan_store_capacity_exhausted && represented {
                        let lease = lease.as_ref().ok_or_else(|| {
                            AppError::Invariant(
                                "scanner emitted an unleased sealed coverage result".to_string(),
                            )
                        })?;
                        for run in input_runs {
                            if let Err(error) = scan_store.accept_leased_input_run(lease, run) {
                                let message = error.to_string();
                                if !is_scan_store_capacity_failure(&message) {
                                    return Err(AppError::Model(message));
                                }
                                scan_store_capacity_exhausted = true;
                                scan_store
                                    .discard_active()
                                    .map_err(|error| AppError::Model(error.to_string()))?;
                                summary.unscanned_entries =
                                    summary.unscanned_entries.saturating_add(1);
                                summary.last_unscanned_reason = Some(message.clone());
                                summary.last_worker_error = Some(message);
                                break;
                            }
                        }
                    }
                    if !represented {
                        scan_store.record_unrecorded_path();
                    }
                    summary.unscanned_entries = summary.unscanned_entries.saturating_add(1);
                    summary.last_unscanned_path = Some(safe_display_path_text(&path));
                    summary.last_unscanned_reason = Some(display_reason(&reason));
                    match &reason {
                        crate::model::UnscannedReason::Excluded(_) => {
                            summary.excluded_entries = summary.excluded_entries.saturating_add(1);
                        }
                        crate::model::UnscannedReason::FilesystemBoundary => {
                            summary.filesystem_boundaries =
                                summary.filesystem_boundaries.saturating_add(1);
                        }
                        crate::model::UnscannedReason::SymbolicLink => {
                            summary.link_entries = summary.link_entries.saturating_add(1);
                        }
                        crate::model::UnscannedReason::Metadata(message)
                        | crate::model::UnscannedReason::Replacement(message) => {
                            summary.unreadable_entries =
                                summary.unreadable_entries.saturating_add(1);
                            summary
                                .last_unreadable_path
                                .clone_from(&summary.last_unscanned_path);
                            summary.last_worker_error = Some(safe_display_text(message));
                        }
                        crate::model::UnscannedReason::IdentityStorageCapacity => {}
                    }
                }
                WorkerEvent::ScanFailed { path, message } => {
                    let capacity_exhausted = is_scan_store_capacity_failure(&message);
                    let message = safe_display_text(&message);
                    if capacity_exhausted {
                        if !scan_store_capacity_exhausted {
                            scan_store_capacity_exhausted = true;
                            scan_store
                                .discard_active()
                                .map_err(|error| AppError::Model(error.to_string()))?;
                            summary.unscanned_entries = summary.unscanned_entries.saturating_add(1);
                            summary.last_unscanned_path =
                                path.as_deref().map(safe_display_path_text);
                            summary.last_unscanned_reason = Some(message.clone());
                            summary.last_worker_error = Some(message);
                        }
                    } else {
                        scan_store.record_unrecorded_path();
                        summary.unscanned_entries = summary.unscanned_entries.saturating_add(1);
                        summary.unreadable_entries = summary.unreadable_entries.saturating_add(1);
                        summary.last_unscanned_path = path.as_deref().map(safe_display_path_text);
                        summary
                            .last_unreadable_path
                            .clone_from(&summary.last_unscanned_path);
                        summary.last_unscanned_reason = Some(message.clone());
                        summary.last_worker_error = Some(message);
                    }
                }
                WorkerEvent::ScanFinished { cancelled } => return Ok(cancelled),
                WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
    })();
    let shutdown_result = workers.shutdown();
    let cancelled = scan_result?;
    shutdown_result?;
    if cancelled {
        return Ok(OperationOutcome::Cancelled {
            value: Some(ScanReport::cancelled(
                settings.root,
                Some(settings.root_identity),
                summary,
            )),
            precise: true,
        });
    }
    let (used, limit) = scan_store.storage_stats();
    summary.scan_store_bytes = used;
    summary.scan_store_limit_bytes = limit;
    if scan_store_capacity_exhausted {
        return Ok(summary_only_scan_outcome(
            settings.root,
            settings.root_identity,
            summary,
        ));
    }
    let reduction = coordinator
        .acquire(
            WorkKind::ReduceRun,
            RelativePath::root(),
            WorkPriority::Reducer,
        )
        .map_err(|error| AppError::Worker(error.to_string()))?
        .ok_or_else(|| AppError::Invariant("headless reduction was not admitted".to_string()))?;
    if let Err(error) = scan_store.publish() {
        let _ = coordinator.finish(reduction, WorkCompletion::Failed);
        return Err(AppError::Model(error.to_string()));
    }
    if coordinator
        .finish(reduction, WorkCompletion::Succeeded)
        .map_err(|error| AppError::Worker(error.to_string()))?
        != crate::scan_coordinator::CompletionOutcome::Accepted
    {
        return Err(AppError::Invariant(
            "headless reduction lease was no longer active".to_string(),
        ));
    }
    let (used, limit) = scan_store.storage_stats();
    summary.scan_store_bytes = used;
    summary.scan_store_limit_bytes = limit;
    if scan_store.is_summary_only() {
        let summary_generation = scan_store
            .into_summary_only()
            .map_err(|error| AppError::Model(error.to_string()))?;
        let report = ScanReport::summary_only_with_root(
            settings.root,
            Some(settings.root_identity),
            summary_generation.root_metrics(),
            summary_generation.root_coverage(),
            summary.clone(),
        );
        return Ok(OperationOutcome::Partial {
            completed_entries: summary.scanned_entries,
            failed_entries: summary.unscanned_entries,
            value: report,
        });
    }
    let published = scan_store
        .into_published()
        .map_err(|error| AppError::Model(error.to_string()))?;
    let state = canonical_scan_report_state(&published, &summary, false);
    let uncertain = state == ScanReportState::Uncertain;
    let report = ScanReport::from_published_generation(
        settings.root,
        Some(settings.root_identity),
        published,
        summary.clone(),
        state,
    );
    if uncertain {
        Ok(OperationOutcome::Uncertain {
            unreadable_entries: summary.unreadable_entries,
            value: report,
        })
    } else {
        Ok(OperationOutcome::Exact(report))
    }
}

fn next_export_path(kind: &str) -> Result<PathBuf, String> {
    let directory = std::env::current_dir().map_err(|error| error.to_string())?;
    for suffix in 0..1_000_u16 {
        let name = if suffix == 0 {
            format!("excise-{kind}.json")
        } else {
            format!("excise-{kind}-{suffix}.json")
        };
        let path = directory.join(name);
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(format!("no free export filename for {kind}"))
}

#[must_use]
pub const fn outcome_exit_class(outcome: &OperationOutcome<RunSummary>) -> ExitClass {
    outcome.exit_class()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    struct PendingInput;

    impl InputSource for PendingInput {
        fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
            Ok(true)
        }

        fn read(&mut self) -> Result<InputEvent, AppError> {
            panic!("pending input must not be read while processing worker events");
        }
    }

    struct IdleInput;

    impl InputSource for IdleInput {
        fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
            Ok(false)
        }

        fn read(&mut self) -> Result<InputEvent, AppError> {
            panic!("idle input must not be read")
        }
    }

    struct InputAfterFirstPoll {
        polls: u8,
    }

    impl InputSource for InputAfterFirstPoll {
        fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
            let pending = self.polls > 0;
            self.polls = self.polls.saturating_add(1);
            Ok(pending)
        }

        fn read(&mut self) -> Result<InputEvent, AppError> {
            panic!("input must only be observed while processing worker events");
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression constructs a complete worker event handoff before asserting input priority"
    )]
    fn pending_input_yields_before_a_scan_batch() {
        let root = tempfile::tempdir().expect("test root should be created");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"x").expect("test entry should be created");
        let root_metadata =
            std::fs::symlink_metadata(root.path()).expect("test root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &root_metadata)
            .expect("test root identity should be readable")
            .expect("test root should not be a link");
        let app = App::new_with_root_identity(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            root_identity.clone(),
            false,
            false,
            crate::model::DEFAULT_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let scan_view_root = app.current_folder_path();
        let workers = WorkerPool::start(
            scanner::ScannerOptions {
                session: app.scan_session_id(),
                generation: ScanGeneration::initial(),
                coordinator: app.session_coordinator(),
                root: root.path().to_path_buf(),
                root_identity: Some(root_identity.clone()),
                threads: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                internal_paths: app.internal_scan_paths(),
                temporary_storage: TemporaryStorage::default(),
                input_runs: None,
            },
            1,
        )
        .expect("workers should start");
        let mut owner = OwnerLoop {
            app,
            input: Box::new(PendingInput),
            workers: Some(workers),
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(true, true, Duration::ZERO),
            settings: RuntimeSettings {
                root: root.path().to_path_buf(),
                root_identity,
                scan_threads: 1,
                event_capacity: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                memory_mib: crate::model::DEFAULT_PROCESS_MIB,
                temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
                scan_store_mib: Some(4_096),
                scan_store_reserve_mib: None,
                scan_store_dir: None,
                apparent_size: false,
                disable_delete_confirmation: false,
                reduced_motion: true,
                monochrome: true,
                animate_loading: false,
                theme: ThemeId::ExciseDark,
                ascii: false,
                mouse: false,
                keymap: KeyPreset::Vim,
                custom_keys: None,
                config_path: None,
                monochrome_locked: true,
            },
            scan_store_storage: TemporaryStorage::scan_store_from_mib(4_096)
                .expect("default scan-store capacity should fit"),
            summary: RunSummary::default(),
            scan_active: true,
            scheduler_snapshot: None,
            primary_scan_active: true,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries: VecDeque::new(),
            scan_cancelled: false,
            generation_rebuild_active: false,
            generation_rebuild_target: None,
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: Duration::ZERO,
            next_deletion_progress_frame: Duration::ZERO,
            last_deletion_progress: None,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while owner
            .workers
            .as_ref()
            .is_some_and(|workers| workers.events().is_empty())
        {
            assert!(
                std::time::Instant::now() < deadline,
                "scanner never queued its first event"
            );
            std::thread::sleep(Duration::from_millis(1));
        }

        let processed = owner
            .process_worker_batch()
            .expect("worker batch should yield cleanly");
        let scanned_entries = owner.summary.scanned_entries;
        owner
            .workers
            .take()
            .expect("workers should still be owned")
            .shutdown()
            .expect("workers should shut down");

        assert!(!processed, "queued input must preempt scan work");
        assert_eq!(scanned_entries, 0);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the regression constructs an isolated owner loop and its staged scan batch"
    )]
    #[test]
    fn queued_input_preempts_after_one_scan_entry() {
        let root = tempfile::tempdir().expect("test root should be created");
        let first = root.path().join("first");
        std::fs::write(&first, b"a").expect("test entry should be created");
        let root_metadata =
            std::fs::symlink_metadata(root.path()).expect("test root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &root_metadata)
            .expect("test root identity should be readable")
            .expect("test root should not be a link");
        let app = App::new_with_root_identity(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            root_identity.clone(),
            false,
            false,
            crate::model::DEFAULT_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        let scan_view_root = app.current_folder_path();
        let first_metadata =
            std::fs::symlink_metadata(&first).expect("first test metadata should exist");
        let first_identity = crate::native_path::identity_for(&first, &first_metadata)
            .expect("first test identity should be readable")
            .expect("first test entry should not be a link");
        let mut owner = OwnerLoop {
            app,
            input: Box::new(InputAfterFirstPoll { polls: 0 }),
            workers: None,
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(true, true, Duration::ZERO),
            settings: RuntimeSettings {
                root: root.path().to_path_buf(),
                root_identity,
                scan_threads: 1,
                event_capacity: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                memory_mib: crate::model::DEFAULT_PROCESS_MIB,
                temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
                scan_store_mib: Some(4_096),
                scan_store_reserve_mib: None,
                scan_store_dir: None,
                apparent_size: false,
                disable_delete_confirmation: false,
                reduced_motion: true,
                monochrome: true,
                animate_loading: false,
                theme: ThemeId::ExciseDark,
                ascii: false,
                mouse: false,
                keymap: KeyPreset::Vim,
                custom_keys: None,
                config_path: None,
                monochrome_locked: true,
            },
            scan_store_storage: TemporaryStorage::scan_store_from_mib(4_096)
                .expect("default scan-store capacity should fit"),
            summary: RunSummary::default(),
            scan_active: true,
            scheduler_snapshot: None,
            primary_scan_active: true,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries: VecDeque::new(),
            scan_cancelled: false,
            generation_rebuild_active: false,
            generation_rebuild_target: None,
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: Duration::ZERO,
            next_deletion_progress_frame: Duration::ZERO,
            last_deletion_progress: None,
        };

        owner
            .handle_worker_event(WorkerEvent::ScanBatch {
                lease: None,
                entries: (0..=MAX_SCAN_ENTRIES_PER_SLICE)
                    .map(|index| ScannedEntry {
                        metadata: first_metadata.clone(),
                        path: root.path().join(format!("entry-{index}")),
                        identity: first_identity.clone(),
                    })
                    .collect(),
                input_runs: Vec::new(),
            })
            .expect("scan batch should be staged");
        assert_eq!(owner.summary.scanned_entries, 0);
        assert_eq!(
            owner.pending_scan_entries.len(),
            MAX_SCAN_ENTRIES_PER_SLICE.saturating_add(1)
        );

        assert!(
            owner
                .process_worker_batch()
                .expect("staged scan batch should be applied")
        );
        assert_eq!(
            owner.summary.scanned_entries, 1,
            "an input check occurs after each scan entry"
        );
        assert_eq!(
            owner.pending_scan_entries.len(),
            MAX_SCAN_ENTRIES_PER_SLICE
                .saturating_add(1)
                .saturating_sub(1),
            "an arriving keypress must preempt scan work before the producer batch drains"
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression constructs a complete active scan frame without a worker thread"
    )]
    fn busy_scan_work_keeps_animation_and_rebuild_startup_responsive() {
        let root = tempfile::tempdir().expect("test root should be created");
        let entry = root.path().join("entry");
        std::fs::create_dir(&entry).expect("test directory should be created");
        let root_metadata =
            std::fs::symlink_metadata(root.path()).expect("test root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &root_metadata)
            .expect("test root identity should be readable")
            .expect("test root should not be a link");
        let entry_metadata =
            std::fs::symlink_metadata(&entry).expect("test entry metadata should exist");
        let entry_identity = crate::native_path::identity_for(&entry, &entry_metadata)
            .expect("test entry identity should be readable")
            .expect("test entry should not be a link");
        let mut app = App::new_with_root_identity(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            root_identity.clone(),
            false,
            false,
            crate::model::DEFAULT_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.set_loading_animation_enabled(true);
        let mut animation = AnimationScheduler::new(false, false, Duration::ZERO);
        assert!(
            app.render_if_dirty(
                &mut animation,
                Duration::ZERO,
                "test",
                crate::theme::Theme::for_id(ThemeId::ExciseDark),
                false,
                false,
                false,
            )
            .expect("initial scan field should render")
        );
        let due_frame = animation
            .next_frame_at()
            .expect("animated scan field should schedule a next frame");
        let clock = VirtualClock::new();
        clock.advance(due_frame);
        let scan_view_root = app.current_folder_path();
        let pending_scan_entries = VecDeque::from([ScannedEntry {
            metadata: entry_metadata,
            path: entry.clone(),
            identity: entry_identity,
        }]);
        let mut owner = OwnerLoop {
            app,
            input: Box::new(IdleInput),
            workers: None,
            clock: Box::new(clock),
            animation,
            settings: RuntimeSettings {
                root: root.path().to_path_buf(),
                root_identity,
                scan_threads: 1,
                event_capacity: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                memory_mib: crate::model::DEFAULT_PROCESS_MIB,
                temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
                scan_store_mib: Some(4_096),
                scan_store_reserve_mib: None,
                scan_store_dir: None,
                apparent_size: false,
                disable_delete_confirmation: false,
                reduced_motion: false,
                monochrome: false,
                animate_loading: true,
                theme: ThemeId::ExciseDark,
                ascii: false,
                mouse: false,
                keymap: KeyPreset::Vim,
                custom_keys: None,
                config_path: None,
                monochrome_locked: false,
            },
            scan_store_storage: TemporaryStorage::scan_store_from_mib(4_096)
                .expect("default scan-store capacity should fit"),
            summary: RunSummary::default(),
            scan_active: true,
            scheduler_snapshot: None,
            primary_scan_active: true,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries,
            scan_cancelled: false,
            generation_rebuild_active: false,
            generation_rebuild_target: None,
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: due_frame,
            next_deletion_progress_frame: due_frame,
            last_deletion_progress: None,
        };

        assert!(
            owner
                .process_worker_batch()
                .expect("staged scan work should be processed")
        );
        assert_eq!(owner.summary.scanned_entries, 1);
        assert!(
            owner
                .animation
                .next_frame_at()
                .is_some_and(|next| next > due_frame),
            "a due scan field frame must be rendered before the busy scan batch returns"
        );
    }
    #[cfg(any(unix, windows))]
    fn complete_queued_deletion(
        app: &mut App<TestBackend>,
        root: &std::path::Path,
    ) -> (crate::state::deletion_work::DeletionWorkId, DeletionReport) {
        let target = app
            .request_deletion()
            .expect("rendered target should delete");
        let plan = crate::deletion::build_plan(root, target.clone(), false)
            .expect("target deletion plan should build");
        assert!(app.queue_deletion_confirmation(target, false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        let (work_id, confirmed) = app
            .arm_and_confirm_deletion_target()
            .expect("confirmation should arm deletion work");
        assert!(app.queue_confirmed_deletion(work_id, confirmed, Duration::ZERO));
        let _planning = app
            .next_deletion_planning_work()
            .expect("confirmed deletion should queue planning");
        assert!(app.deletion_plan_ready(work_id, Box::new(plan)));
        let execution = app
            .next_deletion_execution_work()
            .expect("planned deletion should queue execution");
        let crate::state::deletion_work::DeletionWorkCommand::Execute { plan, .. } = execution
        else {
            panic!("queued deletion should enter its execution lane");
        };
        let report = crate::deletion::execute_plan(
            root,
            *plan,
            &std::sync::atomic::AtomicBool::new(false),
            &std::sync::atomic::AtomicBool::new(false),
        );
        assert!(report.target_was_removed());
        (work_id, report)
    }

    #[cfg(any(unix, windows))]
    fn owner_for_completed_deletion(
        app: App<TestBackend>,
        root: &std::path::Path,
        root_identity: NativeIdentity,
    ) -> OwnerLoop<TestBackend> {
        let scan_view_root = app.current_folder_path();
        OwnerLoop {
            app,
            input: Box::new(PendingInput),
            workers: None,
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(false, false, Duration::ZERO),
            settings: RuntimeSettings {
                root: root.to_path_buf(),
                root_identity,
                scan_threads: 1,
                event_capacity: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                memory_mib: crate::model::MIN_PROCESS_MIB,
                temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
                scan_store_mib: Some(4_096),
                scan_store_reserve_mib: None,
                scan_store_dir: None,
                apparent_size: false,
                disable_delete_confirmation: false,
                reduced_motion: false,
                monochrome: false,
                animate_loading: false,
                theme: ThemeId::ExciseDark,
                ascii: false,
                mouse: false,
                keymap: KeyPreset::Vim,
                custom_keys: None,
                config_path: None,
                monochrome_locked: false,
            },
            scan_store_storage: TemporaryStorage::scan_store_from_mib(4_096)
                .expect("default scan-store capacity should fit"),
            summary: RunSummary::default(),
            scan_active: false,
            scheduler_snapshot: None,
            primary_scan_active: false,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries: VecDeque::new(),
            scan_cancelled: false,
            generation_rebuild_active: false,
            generation_rebuild_target: None,
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: Duration::ZERO,
            next_deletion_progress_frame: Duration::ZERO,
            last_deletion_progress: None,
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn completed_target_reflows_immediately_while_its_copied_tile_departs() {
        let root = tempfile::tempdir().expect("test root should be created");
        let target_path = root.path().join("target");
        std::fs::write(&target_path, vec![b'x'; 8 * 1024]).expect("test target should be created");
        let survivor_path = root.path().join("survivor");
        std::fs::write(&survivor_path, b"keep").expect("test survivor should be created");
        let root_metadata =
            std::fs::symlink_metadata(root.path()).expect("test root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &root_metadata)
            .expect("test root identity should be readable")
            .expect("test root should not be a link");
        let target_metadata =
            std::fs::symlink_metadata(&target_path).expect("test target metadata should exist");
        let target_identity = crate::native_path::identity_for(&target_path, &target_metadata)
            .expect("test target identity should be readable")
            .expect("test target should not be a link");
        let survivor_metadata =
            std::fs::symlink_metadata(&survivor_path).expect("test survivor metadata should exist");
        let survivor_identity =
            crate::native_path::identity_for(&survivor_path, &survivor_metadata)
                .expect("test survivor identity should be readable")
                .expect("test survivor should not be a link");
        let mut app = App::new_with_root_identity(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            root_identity.clone(),
            false,
            false,
            crate::model::MIN_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.append_scan_store_entry_for_test(&target_metadata, &target_path, &target_identity);
        app.append_scan_store_entry_for_test(
            &survivor_metadata,
            &survivor_path,
            &survivor_identity,
        );
        app.finalize_scan();
        app.start_ui();
        let mut initial_animation = AnimationScheduler::new(true, false, Duration::ZERO);
        app.render_if_dirty(
            &mut initial_animation,
            Duration::ZERO,
            "test",
            crate::theme::Theme::for_id(ThemeId::ExciseDark),
            false,
            false,
            true,
        )
        .expect("target should render into the map");

        let (work_id, report) = complete_queued_deletion(&mut app, root.path());
        assert!(!target_path.exists());
        let mut owner = owner_for_completed_deletion(app, root.path(), root_identity);

        owner
            .handle_worker_event(WorkerEvent::DeletionFinished { work_id, report })
            .expect("deletion completion should enter the departure state");
        assert!(owner.app.has_deletion_departure());
        assert!(owner.app.deletion_departure_deadline().is_some());
        assert!(!owner.app.deletion_work.has_work());
        assert!(!owner.app.can_exit_immediately());
        let next_target = owner
            .app
            .request_deletion()
            .expect("surviving node should be selectable before departure completes");
        assert_eq!(next_target.full_path(), survivor_path);

        let departure_deadline = owner
            .app
            .deletion_departure_deadline()
            .expect("departure should have a deadline");
        assert!(owner.clock.advance_to(departure_deadline));
        assert!(
            owner
                .process_deletion_departure(false)
                .expect("departure deadline should clear the copied tile")
        );
        assert!(owner.app.deletion_departure_deadline().is_none());
        assert!(owner.app.can_exit_immediately());
        let next_target = owner
            .app
            .request_deletion()
            .expect("surviving node should remain selectable after departure cleanup");
        assert_eq!(next_target.full_path(), survivor_path);
    }

    #[allow(clippy::too_many_lines)]
    #[test]
    fn generation_rebuild_lifecycle_does_not_schedule_header_completion() {
        let root = tempfile::tempdir().expect("scan rebuild root should exist");
        let root_metadata = std::fs::symlink_metadata(root.path())
            .expect("scan rebuild root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &root_metadata)
            .expect("scan rebuild root identity should be readable")
            .expect("scan rebuild root should not be a link");
        let mut app = App::new_with_root_identity(
            TestBackend::new(80, 24),
            root.path().to_path_buf(),
            root_identity.clone(),
            false,
            false,
            crate::model::DEFAULT_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("scan rebuild app should initialize");
        app.finalize_scan();
        app.start_ui();
        app.require_generation_rebuild_for_test();
        assert!(
            app.begin_generation_rebuild()
                .expect("rebuild should begin")
        );
        let scan_view_root = app.current_folder_path();
        let mut owner = OwnerLoop {
            app,
            input: Box::new(PendingInput),
            workers: None,
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(false, false, Duration::ZERO),
            settings: RuntimeSettings {
                root: root.path().to_path_buf(),
                root_identity,
                scan_threads: 1,
                event_capacity: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                memory_mib: crate::model::DEFAULT_PROCESS_MIB,
                temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
                scan_store_mib: Some(4_096),
                scan_store_reserve_mib: None,
                scan_store_dir: None,
                apparent_size: false,
                disable_delete_confirmation: false,
                reduced_motion: false,
                monochrome: false,
                animate_loading: false,
                theme: ThemeId::ExciseDark,
                ascii: false,
                mouse: false,
                keymap: KeyPreset::Vim,
                custom_keys: None,
                config_path: None,
                monochrome_locked: false,
            },
            scan_store_storage: TemporaryStorage::scan_store_from_mib(4_096)
                .expect("default scan-store capacity should fit"),
            summary: RunSummary::default(),
            scan_active: true,
            scheduler_snapshot: None,
            primary_scan_active: false,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries: VecDeque::new(),
            scan_cancelled: false,
            generation_rebuild_active: true,
            generation_rebuild_target: Some(root.path().to_path_buf()),
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: Duration::ZERO,
            next_deletion_progress_frame: Duration::ZERO,
            last_deletion_progress: None,
        };

        owner
            .handle_worker_event(WorkerEvent::ScanFinished { cancelled: true })
            .expect("cancelled scan rebuild should settle");
        assert_eq!(
            owner.animation.pending_slots(),
            0,
            "cancelling a scan rebuild must not flash completion through the header"
        );

        owner.app.require_generation_rebuild_for_test();
        assert!(
            owner
                .app
                .begin_generation_rebuild()
                .expect("rebuild should restart")
        );
        owner.generation_rebuild_active = true;
        owner.generation_rebuild_target = Some(root.path().to_path_buf());
        owner.scan_active = true;
        owner
            .handle_worker_event(WorkerEvent::ScanFinished { cancelled: false })
            .expect("completed generation rebuild should settle");
        assert_eq!(
            owner.animation.pending_slots(),
            0,
            "finishing generation work must not flash completion through the header"
        );
    }
    #[test]
    fn scan_store_capacity_returns_a_summary_only_partial_outcome() {
        let root = tempfile::tempdir().expect("scan root should exist");
        let metadata = std::fs::symlink_metadata(root.path()).expect("root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a link");
        let summary = RunSummary {
            scanned_entries: 17,
            unscanned_entries: 1,
            ..RunSummary::default()
        };

        let outcome = summary_only_scan_outcome(root.path().to_path_buf(), root_identity, summary);

        let OperationOutcome::Partial {
            value,
            completed_entries,
            failed_entries,
        } = outcome
        else {
            panic!("scan-store capacity must retain a summary-only partial outcome");
        };
        assert_eq!(completed_entries, 17);
        assert_eq!(failed_entries, 1);
        assert_eq!(value.state(), ScanReportState::SummaryOnly);
        assert!(is_scan_store_capacity_failure(
            "scan store capacity exhausted; increase --scan-store-mib"
        ));
        assert!(!is_scan_store_capacity_failure(
            "temporary storage capacity exhausted; increase --temporary-storage-mib"
        ));
    }

    #[test]
    fn exhausted_scan_store_returns_a_summary_only_report() {
        let root = tempfile::tempdir().expect("scan root should exist");
        for index in 0..32 {
            std::fs::write(root.path().join(format!("entry-{index:02}")), b"payload")
                .expect("fixture entry should be written");
        }
        let metadata = std::fs::symlink_metadata(root.path()).expect("root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &metadata)
            .expect("root identity should be readable")
            .expect("root should not be a link");
        let settings = RuntimeSettings {
            root: root.path().to_path_buf(),
            root_identity,
            scan_threads: 1,
            event_capacity: 8,
            cross_filesystems: false,
            exclusions: Vec::new(),
            memory_mib: crate::model::MIN_PROCESS_MIB,
            temporary_storage_mib: crate::temporary_storage::MIN_TEMPORARY_STORAGE_MIB,
            scan_store_mib: Some(crate::temporary_storage::MIN_SCAN_STORE_MIB),
            scan_store_reserve_mib: None,
            scan_store_dir: None,
            apparent_size: false,
            disable_delete_confirmation: false,
            reduced_motion: true,
            monochrome: true,
            animate_loading: false,
            theme: ThemeId::ExciseDark,
            ascii: false,
            mouse: false,
            keymap: KeyPreset::Vim,
            custom_keys: None,
            config_path: None,
            monochrome_locked: true,
        };

        let outcome = scan_headless_with_scan_store_storage(
            settings,
            TemporaryStorage::scan_store_with_limit_bytes(512),
        )
        .expect("capacity exhaustion should produce a terminal summary");

        let OperationOutcome::Partial {
            value,
            failed_entries,
            ..
        } = outcome
        else {
            panic!("exhausted scan storage must not become a runtime failure");
        };
        assert_eq!(value.state(), ScanReportState::SummaryOnly);
        assert_eq!(failed_entries, 1);
        assert_eq!(value.summary().scan_store_limit_bytes, 512);
        assert!(
            value.summary().scan_store_bytes <= value.summary().scan_store_limit_bytes,
            "summary-only report must preserve a bounded scan-store capacity state"
        );
        assert!(
            value
                .summary()
                .last_worker_error
                .as_deref()
                .is_some_and(is_scan_store_capacity_failure),
            "the retained summary should explain the capacity terminal state"
        );
    }

    #[test]
    fn unchanged_deletion_progress_does_not_request_another_map_frame() {
        let mut previous = None;
        assert!(deletion_progress_changed(&mut previous, 8, 0));
        assert!(!deletion_progress_changed(&mut previous, 8, 0));
        assert!(deletion_progress_changed(&mut previous, 8, 1));
        assert!(deletion_progress_changed(&mut previous, 16, 1));
    }

    #[test]
    fn storage_limited_deletion_plan_explains_the_remedy() {
        let error = DeletionPlanError::Io {
            path: "target".to_string(),
            message: "temporary storage capacity exhausted".to_string(),
            kind: std::io::ErrorKind::StorageFull,
        };

        assert_eq!(
            deletion_plan_failure_notice(&error),
            "Deletion did not start: temporary storage limit reached; increase --temporary-storage-mib"
        );
    }

    #[test]
    fn export_notice_preserves_deceptive_path_marker() {
        let path = Path::new("report-\u{202e}name\u{1b}[31m.json");
        let rendered = export_notice("Scan report exported to", path);
        assert!(rendered.starts_with("Scan report exported to [deceptive]"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(rendered.contains("\\x1b"));
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains('\u{202e}'));
    }

    #[cfg(unix)]
    #[test]
    fn export_notice_preserves_invalid_native_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let path = PathBuf::from(OsString::from_vec(b"report-\xff.json".to_vec()));
        let rendered = export_notice("Deletion history exported to", &path);
        assert!(rendered.contains("[deceptive]"));
        assert!(rendered.contains("report-\\xff.json"));
    }

    #[test]
    fn runtime_unscanned_reasons_are_safe_display_text() {
        let reason =
            crate::model::UnscannedReason::Metadata("metadata failed\t\u{202e}name".to_string());
        let rendered = display_reason(&reason);
        assert!(rendered.contains("[deceptive]"));
        assert!(rendered.contains("\\t"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains('\u{202e}'));
    }
}
