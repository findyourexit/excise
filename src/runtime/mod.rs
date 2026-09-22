mod clock;
mod scanner;
mod worker;

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

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
use crate::config::{CustomKeyBindings, KeyPreset, SafePreferences, save_safe_preferences};
use crate::error::{AppError, ExitClass};
use crate::input::{InputCommand, InputEvent, InputSource, handle_keypress};
use crate::native_path::{
    DECEPTIVE_DISPLAY_MARKER, NativeIdentity, safe_display_path_text, safe_display_text,
};
use crate::outcome::{OperationOutcome, RunSummary};
use crate::report::{ScanReport, ScanReportState, scan_report_state};
use crate::state::files::FileTree;
use crate::temporary_storage::TemporaryStorage;
use crate::theme::ThemeId;

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const IDLE_INPUT_WAIT: Duration = Duration::from_hours(1);
const LOADING_FRAME_INTERVAL: Duration = Duration::from_millis(100);
const TRANSIENT_STATUS_DURATION: Duration = Duration::from_millis(250);
const DELETION_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INPUT_BATCH: usize = 32;

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
    temporary_storage: TemporaryStorage,
    summary: RunSummary,
    scan_active: bool,
    /// The initial breadth-first scan remains active while an on-demand scan may run.
    primary_scan_active: bool,
    /// Scan data relevant to the displayed folder arrived since its last refresh.
    scan_view_dirty: bool,
    /// Folder whose incoming scan changes may refresh the visible map.
    scan_view_root: PathBuf,
    /// One scanner batch, drained a single model mutation at a time.
    pending_scan_entries: VecDeque<ScannedEntry>,
    /// One focused scanner batch, drained ahead of unrelated primary scan data.
    pending_focused_scan_entries: VecDeque<ScannedEntry>,
    scan_cancelled: bool,
    rescan_active: bool,
    /// Root currently owned by the focused scanner, if any.
    rescan_target: Option<PathBuf>,
    cancelled_while_scanning: bool,
    exit_after_work: bool,
    timed_actions: Vec<ScheduledAction>,
    next_loading_frame: Duration,
    /// Live mutation progress needs redraws even when accessibility disables animation.
    next_deletion_progress_frame: Duration,
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
    let app = App::new_with_root_identity_and_temporary_storage(
        terminal_backend,
        settings.root.clone(),
        settings.root_identity.clone(),
        settings.apparent_size,
        settings.disable_delete_confirmation,
        settings.memory_mib,
        settings.keymap,
        settings.custom_keys.clone(),
        settings.mouse,
        temporary_storage.clone(),
    )?;
    let workers = WorkerPool::start(
        scanner::ScannerOptions {
            root: settings.root.clone(),
            root_identity: Some(settings.root_identity.clone()),
            threads: settings.scan_threads,
            cross_filesystems: settings.cross_filesystems,
            exclusions: settings.exclusions.clone(),
            internal_paths: app.internal_scan_paths(),
            temporary_storage: temporary_storage.clone(),
        },
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
        temporary_storage,
        summary: RunSummary::default(),
        scan_active: true,
        primary_scan_active: true,
        scan_view_dirty: false,
        scan_view_root,
        pending_scan_entries: VecDeque::new(),
        pending_focused_scan_entries: VecDeque::new(),
        scan_cancelled: false,
        rescan_active: false,
        rescan_target: None,
        cancelled_while_scanning: false,
        exit_after_work: false,
        timed_actions: Vec::new(),
        next_loading_frame: now.saturating_add(LOADING_FRAME_INTERVAL),
        next_deletion_progress_frame: now.saturating_add(DELETION_PROGRESS_INTERVAL),
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
            InputCommand::StartRescan(target) => {
                if self.rescan_active {
                    return Ok(());
                }
                self.start_manual_rescan(target)?;
            }
            InputCommand::CancelRescan => {
                if self.rescan_active {
                    self.workers()?.cancel_rescan();
                }
            }
            InputCommand::RequestDeletion(target) => {
                let reduced_guardrails = self.app.reduced_deletion_guardrails();
                let maximum_bytes = self.app.maximum_deletion_plan_bytes();
                if self
                    .app
                    .queue_deletion_confirmation(*target, reduced_guardrails, maximum_bytes)
                {
                    self.app.show_next_deletion_confirmation();
                }
            }
            InputCommand::CancelDeletionConfirmation => {
                self.app.cancel_deletion_confirmation();
            }
            InputCommand::ConfirmDeletion { work_id, target } => {
                let target_path = target.full_path();
                let overlaps_focused_scan = self.rescan_target.as_ref().is_some_and(|root| {
                    target_path.starts_with(root) || root.starts_with(&target_path)
                });
                if self.app.queue_confirmed_deletion(work_id, target) {
                    if overlaps_focused_scan {
                        self.workers()?.cancel_rescan();
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
                if selected != original {
                    self.app.preferences_changed();
                }
            }
            InputCommand::PromptExit => self.app.prompt_exit(self.exit_work()),
            InputCommand::CancelPendingWorkAndExit => self.cancel_pending_work_and_exit(false)?,
            InputCommand::StopDeletionAndExit => self.cancel_pending_work_and_exit(true)?,
            InputCommand::SavePreferencesAndExit => {
                let result = self.settings.config_path.as_ref().map_or_else(
                    || {
                        Err(AppError::Config(
                            "no writable config path is available".to_string(),
                        ))
                    },
                    |path| {
                        save_safe_preferences(
                            path,
                            SafePreferences {
                                theme: self.settings.theme,
                                ascii: self.settings.ascii,
                                mouse: self.settings.mouse,
                                keymap: self.settings.keymap,
                                custom_keys: self.settings.custom_keys.clone(),
                                reduced_motion: self.settings.reduced_motion,
                            },
                        )
                    },
                );
                match result {
                    Ok(()) => {
                        self.app.preferences_saved();
                        self.app.exit();
                    }
                    Err(error) => self.app.show_error(error.to_string()),
                }
            }
            InputCommand::DiscardPreferencesAndExit => self.app.exit(),
        }
        if drilled {
            self.scan_view_root = self.app.current_folder_path();
            if self.primary_scan_active {
                self.workers()?.prioritize_scan(&self.scan_view_root)?;
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

    fn start_manual_rescan(&mut self, target: PathBuf) -> Result<(), AppError> {
        let root_identity = self.app.identity_for_path(&target);
        let focus_target = target.clone();
        self.app.begin_rescan(target.clone())?;
        self.scan_view_root = self.app.current_folder_path();
        self.workers()?.request_rescan(scanner::ScannerOptions {
            root: target,
            root_identity,
            threads: self.settings.scan_threads,
            cross_filesystems: self.settings.cross_filesystems,
            exclusions: self.settings.exclusions.clone(),
            internal_paths: self.app.internal_scan_paths(),
            temporary_storage: self.temporary_storage.clone(),
        })?;
        self.scan_active = true;
        self.rescan_active = true;
        self.rescan_target = Some(focus_target);
        self.next_loading_frame = self.clock.now().saturating_add(LOADING_FRAME_INTERVAL);
        Ok(())
    }

    fn start_next_deletion_planning(&mut self) -> Result<(), AppError> {
        let Some(command) = self.app.next_deletion_planning_work() else {
            return Ok(());
        };
        self.submit_deletion_work(command)
    }

    fn start_next_deletion_execution(&mut self) -> Result<(), AppError> {
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
        if !self.exit_after_work || self.rescan_active || self.app.deletion_work.has_work() {
            return;
        }
        self.exit_after_work = false;
        if self.app.preferences_dirty() {
            self.app.prompt_exit(ExitWork::None);
        } else {
            self.app.exit();
        }
    }

    fn process_worker_batch(&mut self) -> Result<bool, AppError> {
        if self.app.map_is_transitioning() || self.input.poll(Duration::ZERO)? {
            return Ok(false);
        }
        if self.process_pending_scan_entry()? {
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

    fn process_pending_scan_entry(&mut self) -> Result<bool, AppError> {
        if let Some(entry) = self.pending_focused_scan_entries.pop_front() {
            self.handle_focused_scan_entry(entry)?;
            return Ok(true);
        }
        let Some(entry) = self.pending_scan_entries.pop_front() else {
            return Ok(false);
        };
        self.handle_primary_scan_entry(entry)?;
        Ok(true)
    }

    fn handle_primary_scan_entry(&mut self, entry: ScannedEntry) -> Result<(), AppError> {
        if self.app.primary_scan_path_is_stale(&entry.path) {
            return Ok(());
        }
        self.scan_view_dirty |= entry.path.starts_with(&self.scan_view_root);
        self.summary.scanned_entries = self.summary.scanned_entries.saturating_add(1);
        self.app
            .add_entry_to_base_folder(&entry.metadata, entry.path, entry.identity)?;
        self.summary.identified_entries =
            u64::try_from(self.app.identity_count()).unwrap_or(u64::MAX);
        Ok(())
    }

    fn handle_focused_scan_entry(&mut self, entry: ScannedEntry) -> Result<(), AppError> {
        self.app
            .add_entry_to_focused_folder(&entry.metadata, entry.path, entry.identity)
    }

    fn handle_primary_unscanned(
        &mut self,
        path: &Path,
        reason: crate::model::UnscannedReason,
    ) -> Result<(), AppError> {
        if self.app.primary_scan_path_is_stale(path) {
            return Ok(());
        }
        self.scan_view_dirty |= path.starts_with(&self.scan_view_root);
        self.summary.unscanned_entries = self.summary.unscanned_entries.saturating_add(1);
        self.summary.last_unscanned_path = Some(safe_display_path_text(path));
        self.summary.last_unscanned_reason = Some(display_reason(&reason));
        match &reason {
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
            crate::model::UnscannedReason::IdentityStorageCapacity
            | crate::model::UnscannedReason::MemoryAggregation => {}
        }
        self.app.record_unscanned(path, reason)
    }
    fn handle_focused_unscanned(
        &mut self,
        path: &Path,
        reason: crate::model::UnscannedReason,
    ) -> Result<(), AppError> {
        self.app.record_focused_unscanned(path, reason)
    }

    fn handle_scan_failure(
        &mut self,
        path: Option<&Path>,
        message: &str,
        focused: bool,
    ) -> Result<(), AppError> {
        if !focused && path.is_some_and(|path| self.app.primary_scan_path_is_stale(path)) {
            return Ok(());
        }
        if !focused {
            self.scan_view_dirty |= path.is_some_and(|path| path.starts_with(&self.scan_view_root));
        }
        let message = safe_display_text(message);
        self.summary.unscanned_entries = self.summary.unscanned_entries.saturating_add(1);
        self.summary.unreadable_entries = self.summary.unreadable_entries.saturating_add(1);
        self.summary.last_unscanned_path = path.map(safe_display_path_text);
        self.summary.last_unreadable_path = self.summary.last_unscanned_path.clone();
        self.summary.last_unscanned_reason = Some(message.clone());
        self.summary.last_worker_error = Some(message.clone());
        if let Some(path) = path {
            let reason = crate::model::UnscannedReason::Metadata(message);
            if focused {
                self.app.record_focused_unscanned(path, reason)?;
            } else {
                self.app.record_unscanned(path, reason)?;
            }
        }
        if !focused {
            self.app.increment_failed_to_read();
        }
        self.animation.schedule_error();
        Ok(())
    }

    fn finish_primary_scan(&mut self, cancelled: bool) -> Result<(), AppError> {
        self.primary_scan_active = false;
        self.scan_view_dirty = false;
        if !self.rescan_active {
            self.scan_active = false;
        }
        self.scan_cancelled = cancelled;
        if !cancelled {
            self.app.finalize_scan()?;
            let (used, limit, spilled) = self.app.model_stats();
            self.summary.model_bytes = used;
            self.summary.model_limit_bytes = limit;
            self.summary.identity_spilled = spilled;
            self.app.start_ui();
            self.animation.schedule_completion();
        }
        Ok(())
    }

    fn finish_focused_scan(&mut self, cancelled: bool) -> Result<(), AppError> {
        self.rescan_active = false;
        self.rescan_target = None;
        if !self.primary_scan_active {
            self.scan_active = false;
        }
        if cancelled {
            self.app.cancel_rescan()?;
        } else {
            self.app.finish_rescan()?;
        }
        let (used, limit, spilled) = self.app.model_stats();
        self.summary.model_bytes = used;
        self.summary.model_limit_bytes = limit;
        self.summary.identity_spilled = spilled;
        self.animation.schedule_completion();
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
                | WorkerEvent::FocusedScanFinished { .. }
        );
        match event {
            WorkerEvent::ScanBatch { entries } => {
                debug_assert!(self.pending_scan_entries.is_empty());
                self.pending_scan_entries.extend(entries);
            }
            WorkerEvent::FocusedScanBatch { entries } => {
                debug_assert!(self.pending_focused_scan_entries.is_empty());
                self.pending_focused_scan_entries.extend(entries);
            }
            WorkerEvent::ScanDirectoryComplete { path, identity } => {
                if !self.app.primary_scan_path_is_stale(&path) {
                    self.scan_view_dirty |= path.starts_with(&self.scan_view_root);
                    self.app.complete_directory(&path, identity.as_ref())?;
                }
            }
            WorkerEvent::FocusedScanDirectoryComplete { path, identity } => {
                self.app
                    .complete_focused_directory(&path, identity.as_ref())?;
            }
            WorkerEvent::ScanUnscanned { path, reason } => {
                self.handle_primary_unscanned(&path, reason)?;
            }
            WorkerEvent::FocusedScanUnscanned { path, reason } => {
                self.handle_focused_unscanned(&path, reason)?;
            }
            WorkerEvent::ScanFailed { path, message } => {
                self.handle_scan_failure(path.as_deref(), &message, false)?;
            }
            WorkerEvent::FocusedScanFailed { path, message } => {
                self.handle_scan_failure(path.as_deref(), &message, true)?;
            }
            WorkerEvent::ScanFinished { cancelled } => self.finish_primary_scan(cancelled)?,
            WorkerEvent::FocusedScanFinished { cancelled } => {
                self.finish_focused_scan(cancelled)?;
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
                Err(_) => {
                    if self.app.deletion_plan_failed(work_id) {
                        self.summary.deletion_failed_entries =
                            self.summary.deletion_failed_entries.saturating_add(1);
                        self.app.record_deletion_notice(
                            "Deletion did not start: the selected item could not be checked",
                        );
                    }
                }
            },
            WorkerEvent::DeletionExecutionRejected { work_id, error } => {
                if error.is_cancelled() {
                    self.app.deletion_execution_finished(work_id);
                    self.app.mark_dirty();
                } else {
                    let notice = if error.is_missing() || error.is_stale() {
                        "Deletion skipped: files changed or disappeared"
                    } else {
                        "Deletion stopped: a final safety check failed"
                    };
                    if self.app.deletion_execution_stale(work_id) {
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
                if self.app.deletion_execution_finished(work_id) {
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
                    match self.app.try_complete_deletion(report) {
                        Ok(true) => {
                            self.animation.schedule_deletion_result();
                            self.app.flash_space_freed();
                            self.schedule(
                                self.clock.now(),
                                TimedAction::UnflashSpace,
                                TRANSIENT_STATUS_DURATION,
                            );
                        }
                        Ok(false) => self.animation.schedule_deletion_result(),
                        Err(error) => {
                            self.summary.unreadable_entries =
                                self.summary.unreadable_entries.saturating_add(1);
                            self.summary.last_worker_error = Some(error.to_string());
                            self.animation.schedule_error();
                        }
                    }
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
            if self.scan_view_dirty && self.app.refresh_board_from_scan() {
                self.scan_view_dirty = false;
            }
            self.next_loading_frame = now.saturating_add(LOADING_FRAME_INTERVAL);
            processed = true;
        }
        let progress_needs_timer = self.app.deletion_work.has_active_mutation()
            && self.animation.next_frame_at().is_none();
        if progress_needs_timer && now >= self.next_deletion_progress_frame {
            self.app.mark_dirty();
            self.next_deletion_progress_frame = now.saturating_add(DELETION_PROGRESS_INTERVAL);
            processed = true;
        } else if !self.app.deletion_work.has_active_mutation() {
            self.next_deletion_progress_frame = now.saturating_add(DELETION_PROGRESS_INTERVAL);
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
        let result = self.app.render_if_dirty(
            &mut self.animation,
            self.clock.now(),
            self.settings.theme.attribution().name,
            crate::theme::Theme::for_id(self.settings.theme),
            self.settings.ascii,
            self.settings.monochrome,
            self.settings.reduced_motion,
        );
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
        if self.app.deletion_work.has_active_mutation() && self.animation.next_frame_at().is_none()
        {
            timeout = timeout.min(self.next_deletion_progress_frame.saturating_sub(now));
        }
        if let Some(deadline) = self.timed_actions.iter().map(|action| action.at).min() {
            timeout = timeout.min(deadline.saturating_sub(now));
        }
        timeout
    }

    fn wait_for_quiescence(&mut self) -> Result<(), AppError> {
        while self.scan_active || self.app.deletion_work.has_background_activity() {
            if self.process_pending_scan_entry()? {
                self.render()?;
                continue;
            }
            match self.workers()?.events().recv_timeout(WORKER_POLL_INTERVAL) {
                Ok(event) => {
                    self.handle_worker_event(event)?;
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
        | crate::model::UnscannedReason::IdentityStorageCapacity
        | crate::model::UnscannedReason::MemoryAggregation => false,
    };
    let rendered = safe_display_text(&format!("{reason:?}"));
    if deceptive && !rendered.contains(DECEPTIVE_DISPLAY_MARKER) {
        format!("{DECEPTIVE_DISPLAY_MARKER} {rendered}")
    } else {
        rendered
    }
}

/// Runs the production scanner and bounded model without acquiring a terminal.
///
/// # Errors
/// Returns a scanner, model, or worker error after all owned workers stop.
#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
pub fn scan_headless(settings: RuntimeSettings) -> Result<OperationOutcome<ScanReport>, AppError> {
    let temporary_storage = TemporaryStorage::from_mib(settings.temporary_storage_mib)
        .map_err(|error| AppError::Config(error.to_string()))?;
    let mut tree = FileTree::new_with_root_identity_and_temporary_storage(
        settings.root.clone(),
        settings.root_identity.clone(),
        settings.apparent_size,
        settings.memory_mib,
        temporary_storage.clone(),
    )
    .map_err(|error| AppError::Model(error.to_string()))?;
    let workers = WorkerPool::start(
        scanner::ScannerOptions {
            root: settings.root.clone(),
            root_identity: Some(settings.root_identity.clone()),
            threads: settings.scan_threads,
            cross_filesystems: settings.cross_filesystems,
            exclusions: settings.exclusions.clone(),
            internal_paths: tree.internal_scan_paths(),
            temporary_storage,
        },
        settings.event_capacity,
    )?;
    let mut summary = RunSummary::default();
    let scan_result = (|| -> Result<bool, AppError> {
        loop {
            let event = workers
                .events()
                .recv()
                .map_err(|_| AppError::Worker("scanner event channel disconnected".to_string()))?;
            match event {
                WorkerEvent::ScanBatch { entries } => {
                    summary.scanned_entries =
                        summary.scanned_entries.saturating_add(entries.len() as u64);
                    for entry in entries {
                        tree.add_entry(&entry.metadata, &entry.path, entry.identity)
                            .map_err(|error| AppError::Model(error.to_string()))?;
                    }
                    summary.identified_entries =
                        u64::try_from(tree.identity_count()).unwrap_or(u64::MAX);
                }
                WorkerEvent::ScanDirectoryComplete { path, identity } => tree
                    .complete_directory(&path, identity.as_ref())
                    .map_err(|error| AppError::Model(error.to_string()))?,
                WorkerEvent::ScanUnscanned { path, reason } => {
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
                            tree.failed_to_read = tree.failed_to_read.saturating_add(1);
                        }
                        crate::model::UnscannedReason::IdentityStorageCapacity
                        | crate::model::UnscannedReason::MemoryAggregation => {}
                    }
                    tree.record_unscanned(&path, reason)
                        .map_err(|error| AppError::Model(error.to_string()))?;
                }
                WorkerEvent::ScanFailed { path, message } => {
                    let message = safe_display_text(&message);
                    summary.unscanned_entries = summary.unscanned_entries.saturating_add(1);
                    summary.unreadable_entries = summary.unreadable_entries.saturating_add(1);
                    summary.last_unscanned_path = path.as_deref().map(safe_display_path_text);
                    summary
                        .last_unreadable_path
                        .clone_from(&summary.last_unscanned_path);
                    summary.last_unscanned_reason = Some(message.clone());
                    summary.last_worker_error = Some(message.clone());
                    if let Some(path) = path {
                        tree.record_unscanned(
                            &path,
                            crate::model::UnscannedReason::Metadata(message),
                        )
                        .map_err(|error| AppError::Model(error.to_string()))?;
                    }
                    tree.failed_to_read = tree.failed_to_read.saturating_add(1);
                }
                WorkerEvent::ScanFinished { cancelled } => return Ok(cancelled),
                WorkerEvent::FocusedScanBatch { .. }
                | WorkerEvent::FocusedScanDirectoryComplete { .. }
                | WorkerEvent::FocusedScanUnscanned { .. }
                | WorkerEvent::FocusedScanFailed { .. }
                | WorkerEvent::FocusedScanFinished { .. } => {
                    return Err(AppError::Invariant(
                        "headless scan received an unexpected focused scan event".to_string(),
                    ));
                }
                WorkerEvent::DeletionPlanned { .. }
                | WorkerEvent::DeletionExecutionRejected { .. }
                | WorkerEvent::DeletionFinished { .. } => {}
            }
        }
    })();
    let shutdown_result = workers.shutdown();
    let cancelled = scan_result?;
    shutdown_result?;
    if !cancelled {
        tree.finalize()
            .map_err(|error| AppError::Model(error.to_string()))?;
    }
    let (used, limit, spilled) = tree.model_stats();
    summary.model_bytes = used;
    summary.model_limit_bytes = limit;
    summary.identity_spilled = spilled;
    let state = scan_report_state(&tree, &summary, cancelled);
    let uncertain = state == ScanReportState::Uncertain;
    let report = ScanReport::from_completed_tree(tree, summary.clone(), state);
    if cancelled {
        Ok(OperationOutcome::Cancelled {
            value: Some(report),
            precise: true,
        })
    } else if uncertain {
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
                root: root.path().to_path_buf(),
                root_identity: Some(root_identity.clone()),
                threads: 1,
                cross_filesystems: false,
                exclusions: Vec::new(),
                internal_paths: app.internal_scan_paths(),
                temporary_storage: TemporaryStorage::default(),
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
            temporary_storage: TemporaryStorage::default(),
            summary: RunSummary::default(),
            scan_active: true,
            primary_scan_active: true,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries: VecDeque::new(),
            pending_focused_scan_entries: VecDeque::new(),
            scan_cancelled: false,
            rescan_active: false,
            rescan_target: None,
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: Duration::ZERO,
            next_deletion_progress_frame: Duration::ZERO,
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
        reason = "the preemption regression constructs an isolated owner loop and its scan batch"
    )]
    #[test]
    fn pending_input_interrupts_a_scan_batch_between_entries() {
        let root = tempfile::tempdir().expect("test root should be created");
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::write(&first, b"a").expect("first test entry should be created");
        std::fs::write(&second, b"b").expect("second test entry should be created");
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
        let second_metadata =
            std::fs::symlink_metadata(&second).expect("second test metadata should exist");
        let second_identity = crate::native_path::identity_for(&second, &second_metadata)
            .expect("second test identity should be readable")
            .expect("second test entry should not be a link");
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
            temporary_storage: TemporaryStorage::default(),
            summary: RunSummary::default(),
            scan_active: true,
            primary_scan_active: true,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries: VecDeque::new(),
            pending_focused_scan_entries: VecDeque::new(),
            scan_cancelled: false,
            rescan_active: false,
            rescan_target: None,
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: Duration::ZERO,
            next_deletion_progress_frame: Duration::ZERO,
        };

        owner
            .handle_worker_event(WorkerEvent::ScanBatch {
                entries: vec![
                    ScannedEntry {
                        metadata: first_metadata,
                        path: first,
                        identity: first_identity,
                    },
                    ScannedEntry {
                        metadata: second_metadata,
                        path: second,
                        identity: second_identity,
                    },
                ],
            })
            .expect("scan batch should be staged");
        assert_eq!(owner.summary.scanned_entries, 0);
        assert_eq!(owner.pending_scan_entries.len(), 2);

        assert!(
            owner
                .process_worker_batch()
                .expect("first scan entry should be processed")
        );
        assert_eq!(owner.summary.scanned_entries, 1);
        assert_eq!(owner.pending_scan_entries.len(), 1);
        assert!(
            !owner
                .process_worker_batch()
                .expect("pending input should preempt the second scan entry")
        );
        assert_eq!(owner.summary.scanned_entries, 1);
        assert_eq!(owner.pending_scan_entries.len(), 1);
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
