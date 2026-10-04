mod clock;
#[cfg(feature = "internal")]
mod probe;
mod scanner;
mod worker;

use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::Instant;

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};
use crossterm::event::Event;
use ratatui::backend::Backend;

#[cfg(any(test, feature = "fuzzing", feature = "internal"))]
pub use clock::VirtualClock;
pub(crate) use clock::{Clock, SystemClock};
#[cfg(feature = "internal")]
pub use probe::{
    HISTOGRAM_BUCKETS, OwnerLoopProbe, OwnerLoopReport, OwnerPhase, PhaseHistogram, WorkerEventKind,
};
use worker::{
    DeletionWorkSubmissionError, HistoryExportError, ScannedEntry, WorkerEvent, WorkerPool,
};

use crate::App;
use crate::animation::AnimationScheduler;
use crate::app::{Announcement, ExitWork, FinishedPublication};
use crate::config::{CustomKeyBindings, KeyPreset, save_theme_preference};
use crate::deletion::{DeletionPlanError, DeletionReport};
use crate::error::{AppError, ExitClass};
use crate::input::{InputCommand, InputEvent, InputSource, handle_keypress};
use crate::native_path::{
    DECEPTIVE_DISPLAY_MARKER, NativeIdentity, safe_display_path_text, safe_display_text,
};
use crate::outcome::{OperationOutcome, RunSummary};
use crate::report::{
    ReportError, ScanReport, ScanReportState, canonical_scan_report_state, write_buffered,
};
use crate::scan_coordinator::{
    RelativePath, ScanGeneration, SchedulerSnapshot, SessionCoordinator, WorkCompletion, WorkKind,
    WorkLease, WorkPriority,
};
use crate::scan_store::session::{
    ScanStore, SealedBatch, is_scan_store_capacity_error, scan_store_capacity_message,
};
use crate::scan_store::storage::ScanStoreStorage;
use crate::signals::{self, ShutdownWait, StopRequest};
use crate::temporary_storage::TemporaryStorage;
use crate::terminal::FrameSink;
use crate::theme::ThemeId;
use crate::ui::palette::ColorCycle;

const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// The longest the loop sleeps while a scan streams in, or the store thread publishes its map.
/// Nothing wakes the loop for the scanner's results (it sleeps in the input poll), and a result
/// arrives every few hundred microseconds: the scanner may have only
/// [`MAX_INFLIGHT_SCAN_BATCHES`](crate::scan_store::session::MAX_INFLIGHT_SCAN_BATCHES) batches
/// sealed and not yet handed on, so a nap of the ordinary interval left it waiting for credits
/// through most of every nap.
const SCAN_POLL_INTERVAL: Duration = Duration::from_millis(1);
/// How often the loop asks the coordinator for its scheduler summary while a scan streams in.
const SCHEDULER_SNAPSHOT_INTERVAL: Duration = Duration::from_millis(50);
const IDLE_INPUT_WAIT: Duration = Duration::from_hours(1);
/// Limits expensive layout rebuilds while a large scan streams in.
const LOADING_FRAME_INTERVAL: Duration = Duration::from_millis(150);
const TRANSIENT_STATUS_DURATION: Duration = Duration::from_millis(250);
const DELETION_PROGRESS_INTERVAL: Duration = Duration::from_millis(100);
const MAX_INPUT_BATCH: usize = 32;
/// A scanner event is capped at 32 entries. Keep one owner slice bounded so
/// scanning can never monopolize the UI loop.
const MAX_SCAN_ENTRIES_PER_SLICE: usize = 32;
/// Upper bound on one blocked input poll while a signal/console-event watcher is attached
/// (`OwnerLoop::stop_signals`), so a confirmed-quit request that arrives while otherwise idle
/// (`IDLE_INPUT_WAIT`) is still noticed promptly instead of only at the next keypress. Comfortably
/// under the 250 ms quit budget; a `poll` that returns on its own timeout spends no measurable
/// CPU while blocked, so this does not reopen the idle budget this program also enforces.
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(200);

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

/// Records one rendered deletion lifecycle state and reports whether the map changed.
fn deletion_progress_changed(
    previous: &mut Option<(u64, u64, bool)>,
    planned: u64,
    completed: u64,
    mutation_started: bool,
) -> bool {
    let current = (planned, completed, mutation_started);
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
        DeletionPlanError::Io {
            kind: std::io::ErrorKind::InvalidFilename,
            ..
        } => {
            "Deletion did not start: a path in the selected item is too long or invalid for the system"
        }
        DeletionPlanError::Unrepresentable { .. } => {
            "Deletion did not start: the file system reported an impossible value for part of the selected item"
        }
        DeletionPlanError::Root => {
            "Deletion did not start: the selected item is or contains a mount point or file system root"
        }
        DeletionPlanError::MemoryLimit { .. } => {
            "Deletion did not start: the selected item exceeds the plan memory limit"
        }
        DeletionPlanError::Io { .. }
        | DeletionPlanError::Synthetic
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
    /// Set only for a real terminal (`cli::run_tui`); gates `render` so a slow terminal cannot
    /// block scan ingestion or input handling. `None` for in-process and test backends (a
    /// `TestBackend` has no terminal to back up), which render exactly as before.
    frame_sink: Option<FrameSink>,
    settings: RuntimeSettings,
    scan_store_storage: TemporaryStorage,
    summary: RunSummary,
    scan_active: bool,
    /// Latest combined session scheduler state. This is not an event backlog.
    scheduler_snapshot: Option<SchedulerSnapshot>,
    /// The primary breadth-first generation remains active while a versioned refresh may run.
    primary_scan_active: bool,
    /// When the loop last asked the coordinator for its scheduler summary.
    scheduler_refreshed_at: Option<Instant>,
    /// The coordinator's reduction lease for the primary scan, held while the store thread
    /// publishes its map: the owner loop finishes it when the publication ends.
    primary_reduction: Option<WorkLease>,
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
    /// Last rendered deletion lifecycle state. Unchanged state does not redraw the map.
    last_deletion_progress: Option<(u64, u64, bool)>,
    /// An external signal (Unix) or console control event (Windows) delivers a confirmed-quit
    /// request here. `None` outside a real terminal or headless process run:
    /// nothing can deliver a real signal to the in-process test or scenario runners.
    stop_signals: Option<Receiver<StopRequest>>,
    /// Set by [`OwnerLoop::begin_signal_quit`] or [`OwnerLoop::force_signal_exit`]. Forces the
    /// run's outcome to [`OperationOutcome::Cancelled`] regardless of scan or deletion state,
    /// because an external signal is always a confirmed quit (the existing `Interrupted` exit
    /// class), even when nothing was scanning or being deleted.
    signal_quit: bool,
    /// Set by [`OwnerLoop::force_signal_exit`]: a second confirmed-quit request ended the run, so
    /// the shutdown that follows waits for a thread that does not end no longer than the grace,
    /// except for the deletion executor, which it always waits for.
    forced_stop: bool,
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
    run_with_frame_gate(terminal_backend, input, settings, clock, None, None)
}

/// Like [`run`], but when `frame_sink` is set, `render` draws a new frame only once the terminal
/// has drained the previous one, so a slow real terminal cannot block scan ingestion or input
/// handling (coalescing: the next frame it does draw reflects however much changed meanwhile).
/// `cli::run_tui` is the only caller that passes one; every in-process caller (tests, benchmarks,
/// the in-process scenario runner) passes `None` and renders exactly as before.
///
/// `stop_signals` carries a confirmed-quit request from an external signal or console control
/// event; `cli::run_tui` is again the only caller that passes one, from the same
/// watcher `cli::run_main` installs for the headless path.
///
/// # Errors
/// Returns a terminal, input, worker, or invariant error after shutting down owned workers.
pub fn run_with_frame_gate<B>(
    terminal_backend: B,
    input: Box<dyn InputSource>,
    settings: RuntimeSettings,
    clock: Box<dyn Clock>,
    frame_sink: Option<FrameSink>,
    stop_signals: Option<Receiver<StopRequest>>,
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
    app.start_scan_store_thread()?;
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
        frame_sink,
        settings,
        scan_store_storage,
        summary: RunSummary::default(),
        scan_active: true,
        scheduler_snapshot: None,
        primary_scan_active: true,
        primary_reduction: None,
        scheduler_refreshed_at: None,
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
        stop_signals,
        signal_quit: false,
        forced_stop: false,
    }
    .run()
}

impl<B> OwnerLoop<B>
where
    B: Backend,
{
    fn run(mut self) -> Result<OperationOutcome<RunSummary>, AppError> {
        let loop_result = self.run_loop();
        // From here this thread is the one waiting for the others, so a second stop request has
        // no loop left to be acted on by: the waits below that a second request bounds watch for
        // it, and the terminal is restored after them whatever they ended with. The wait for the
        // deletion executor is not one of them.
        let mut wait = ShutdownWait::until_forced(self.stop_signals.take(), self.forced_stop);
        let workers = self
            .workers
            .take()
            .ok_or_else(|| AppError::Invariant("worker pool already stopped".to_string()))?;
        let shutdown_result = workers.shutdown_with(&mut wait);
        // The store thread stops last: once the scanner and the deletion workers are gone nothing
        // else writes the session's scratch storage. The directory itself goes when the last
        // holder of that storage, this loop's own app, is dropped.
        let store_result = self.app.shutdown_scan_store(&mut wait);
        let finish_result = self.app.finish();
        let outcome = loop_result?;
        shutdown_result?;
        store_result?;
        finish_result?;
        Ok(outcome)
    }

    fn run_loop(&mut self) -> Result<OperationOutcome<RunSummary>, AppError> {
        self.render()?;
        while self.app.is_running {
            let mut did_work = self.process_stop_signals()?;
            let input_processed = self.process_input_batch()?;
            did_work |= input_processed;
            // A keystroke must be drawn before background work can spend another
            // scheduling slice. This keeps cursor feedback independent of scan load.
            // A confirmed quit draws nothing more: the terminal is restored right after, and a frame
            // queued now would only delay that restoration by however long it takes to drain.
            if input_processed && self.app.is_running {
                did_work |= self.render_reflecting_input()?;
            }
            if !self.app.is_running && self.scan_active {
                self.cancelled_while_scanning = true;
            }
            did_work |= self.process_scan_store_events()?;
            did_work |= self.process_history_export()?;
            did_work |= self.process_worker_batch()?;
            did_work |= self.process_deletion_departure(false)?;
            did_work |= self.process_deadlines();
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
                    // The same rule as at the top of the loop: a keystroke is drawn at once, not
                    // coalesced behind a frame the terminal is still draining.
                    if self.app.is_running {
                        self.render_reflecting_input()?;
                    }
                }
                // A timeout is work too: while `poll` sleeps, geometry and other
                // deadlines become due. Service them even when no key woke us, or
                // a finished scan leaves the map frozen until the next input.
                self.process_deletion_departure(false)?;
                self.process_deadlines();
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
        if self.scan_cancelled || self.cancelled_while_scanning || self.signal_quit {
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
        #[cfg(feature = "internal")]
        let _input = self.probe_phase(OwnerPhase::Input);
        let input = self.input.read()?;
        if matches!(input, InputEvent::Terminal(_)) {
            crate::test_events::input_consumed();
            // Input re-arms the selected tile's sheen for one more cycle (F3).
            self.app.rearm_sheen(self.clock.now());
        }
        let result = match input {
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
                    self.app.suppress_generation_rebuild_restart();
                }
            }
            InputCommand::RequestDeletion(target) => {
                let reduced_guardrails = self.app.reduced_deletion_guardrails();
                let maximum_bytes = self.app.maximum_deletion_plan_bytes();
                #[cfg(feature = "fuzzing")]
                let requested = target.full_path();
                if self.app.queue_deletion_confirmation(
                    *target,
                    reduced_guardrails,
                    maximum_bytes,
                    now,
                ) {
                    #[cfg(feature = "fuzzing")]
                    crate::deletion_probe::requested(
                        &self.app.deletion_work,
                        &requested,
                        reduced_guardrails,
                    );
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
                    #[cfg(feature = "fuzzing")]
                    crate::deletion_probe::confirmed(work_id.0, &target_path);
                    if overlaps_scan_rebuild {
                        self.workers()?.cancel_generation_rebuild();
                    }
                    self.start_next_deletion_planning()?;
                }
            }
            InputCommand::ExportScan => {
                let result = next_export_path("scan-report").and_then(|path| {
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&path)
                        .map_err(|error| error.to_string())?;
                    export_report(file, |writer| {
                        self.app.write_scan_report(&self.summary, writer)
                    })?;
                    Ok(path)
                });
                match result {
                    Ok(path) => self
                        .app
                        .show_notice(export_notice("Scan report exported to", &path)),
                    Err(error) => self.app.show_error(format!("Scan export failed: {error}")),
                }
            }
            InputCommand::ExportDeletionHistory => self.start_history_export()?,
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
            self.scan_view_root = self.app.scan_view_folder_path();
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

    /// Starts exporting the deletion history on a thread of its own
    /// ([`WorkerPool::start_history_export`]): serializing it takes longer than the interface may
    /// go without a frame. The reader sees a notice at once, and another when the file is written
    /// or the export failed. The export goes to the directory the key was pressed in, as the
    /// scan export does.
    fn start_history_export(&mut self) -> Result<(), AppError> {
        let directory = match std::env::current_dir() {
            Ok(directory) => directory,
            Err(error) => {
                self.app
                    .show_error(format!("Deletion history export failed: {error}"));
                return Ok(());
            }
        };
        let reports = self.app.deletion_history_snapshot();
        match self.workers()?.start_history_export(reports, directory) {
            Ok(()) => self.app.show_notice("Exporting the deletion history..."),
            Err(HistoryExportError::Busy) => self
                .app
                .show_notice("The deletion history is already being exported"),
            Err(HistoryExportError::Spawn(error)) => self
                .app
                .show_error(format!("Deletion history export failed: {error}")),
        }
        Ok(())
    }

    /// Reports an export that ended, the file it wrote or why it wrote none, and shows what an
    /// earlier one reported once the reader is free to read it. Returns whether it did either.
    fn process_history_export(&mut self) -> Result<bool, AppError> {
        let shown = self.app.show_waiting_announcement();
        let Some(outcome) = self.workers()?.poll_history_export() else {
            return Ok(shown);
        };
        match outcome.result {
            Ok(path) => {
                self.app.drop_exported_deletion_history(outcome.exported);
                self.app.announce(Announcement::Notice(export_notice(
                    "Deletion history exported to",
                    &path,
                )));
            }
            Err(error) => self.app.announce(Announcement::Error(format!(
                "Deletion history export failed: {error}"
            ))),
        }
        Ok(true)
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
        if let Some((planned_entries, progress)) = self.app.deletion_work.active_progress() {
            return ExitWork::Active {
                planned_entries,
                progress,
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
            if let Some((planned_entries, progress)) = self.app.deletion_work.active_progress() {
                self.workers()?.safely_stop_deletion();
                ExitWork::Stopping {
                    planned_entries,
                    progress,
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

    /// Drains every confirmed-quit request delivered since the last check: the
    /// first begins a graceful quit, and any further one forces an immediate exit instead of
    /// waiting out whatever the first started. `None` outside a real terminal or headless
    /// process run, where nothing can send one. Returns whether any request was seen, so the
    /// caller treats draining them as work done this iteration rather than idling.
    fn process_stop_signals(&mut self) -> Result<bool, AppError> {
        let Some(receiver) = self.stop_signals.clone() else {
            return Ok(false);
        };
        let mut processed = false;
        while let Ok(request) = receiver.try_recv() {
            processed = true;
            match request {
                StopRequest::Graceful => self.begin_signal_quit()?,
                StopRequest::Forced => self.force_signal_exit()?,
            }
        }
        Ok(processed)
    }

    /// Begins a confirmed quit: cancels any pending deletion plan, requests that an
    /// active deletion stop at its next entry boundary, and exits once that settles, exactly
    /// like the interactive "stop and quit" key - but unconditionally, and always forcing the
    /// `Interrupted` exit class (`signal_quit`), even when there is no work to cancel or stop.
    fn begin_signal_quit(&mut self) -> Result<(), AppError> {
        self.signal_quit = true;
        self.cancel_pending_work_and_exit(true)?;
        self.finish_exit_after_work();
        Ok(())
    }

    /// Forces the exit a second confirmed-quit request demands: skips the ordinary wait for a
    /// rescan the interrupted deletion may have scheduled (read-only and not worth waiting out,
    /// unlike an in-flight deletion entry) and any other bookkeeping `finish_exit_after_work`
    /// would otherwise gate on, and ends the run immediately. Filesystem safety for an in-flight
    /// deletion entry still comes from the executor's own entry-boundary check against the stop
    /// this (idempotently, in case both requests were drained together) and the first request
    /// already set, and from `WorkerPool::shutdown_with` always joining that executor, whatever
    /// the stop requests, before the run's process can exit - not from waiting here. A deletion
    /// stuck in a call that never returns therefore still holds a forced exit; the waits for the
    /// other threads are the ones a forced stop bounds ([`signals::FORCED_STOP_GRACE`]).
    fn force_signal_exit(&mut self) -> Result<(), AppError> {
        self.signal_quit = true;
        self.forced_stop = true;
        self.workers()?.safely_stop_deletion();
        self.exit_after_work = false;
        self.app.exit();
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
        // The snapshot is a round trip to the coordinator thread, which answers behind the
        // commands the scan's workers keep sending it: costly, and a wait for the owner loop,
        // when asked on every pass. While a scan streams in, a summary that is at most a frame
        // old is as good, so ask once per interval.
        if self.scan_active
            && self
                .scheduler_refreshed_at
                .is_some_and(|at| at.elapsed() < SCHEDULER_SNAPSHOT_INTERVAL)
        {
            return;
        }
        self.scheduler_refreshed_at = Some(Instant::now());
        let snapshot = self
            .workers
            .as_ref()
            .and_then(WorkerPool::scheduler_snapshot);
        if snapshot != self.scheduler_snapshot {
            self.scheduler_snapshot = snapshot;
            self.app.set_scheduler_snapshot(snapshot);
        }
    }

    /// Applies at most one stored scanner event, unless input is waiting or the map is moving.
    fn process_worker_batch(&mut self) -> Result<bool, AppError> {
        self.refresh_scheduler_snapshot();
        if self.app.map_is_transitioning() || self.input.poll(Duration::ZERO)? {
            return Ok(false);
        }
        if self.process_pending_scan_entries()? {
            return Ok(true);
        }
        if !self.app.scan_store_has_room_for_batch() {
            // The store thread has a backlog of batches to admit. Leave the scanner's events where
            // they are: the scanner backs up through its own bounded channel and the in-flight
            // cap, instead of a queue growing here.
            return Ok(false);
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
        #[cfg(feature = "internal")]
        self.probe_scan_entry();
        self.scan_view_dirty |= entry.path.starts_with(&self.scan_view_root);
        if self.primary_scan_active {
            self.summary.scanned_entries = self.summary.scanned_entries.saturating_add(1);
        }
        self.app.record_loading_entry(entry.path);
    }

    fn admit_coverage_runs(
        &mut self,
        lease: Option<&WorkLease>,
        input_runs: SealedBatch,
    ) -> Result<(), AppError> {
        if input_runs.is_empty() {
            self.app.record_scan_store_unrecorded_path();
            return Ok(());
        }
        let lease = lease.ok_or_else(|| {
            AppError::Invariant("scanner emitted an unleased sealed coverage result".to_string())
        })?;
        #[cfg(feature = "internal")]
        let _admission = self.probe_admission(input_runs.len());
        self.app.admit_scan_input_runs(lease, input_runs);
        Ok(())
    }

    fn handle_primary_unscanned(
        &mut self,
        lease: Option<&WorkLease>,
        input_runs: SealedBatch,
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
        if let Some(path) = path {
            self.app.record_unreadable_directory(path);
        }
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
        self.scan_cancelled = cancelled;
        if cancelled {
            if !self.generation_rebuild_active {
                self.scan_active = false;
            }
            self.app.cancel_primary_scan()?;
            self.record_scan_store_summary();
            self.start_pending_generation_rebuild()?;
            return Ok(());
        }
        // The scanner is done but the map is not: the store thread publishes it. The scan stays
        // open for the reader (the header reads SCANNING, loading frames go on, and a quit counts
        // as cancelled) until `complete_primary_scan` runs when that publication ends. The
        // reduction lease is held across the wait, as it was across the work.
        self.primary_reduction = Some(self.workers()?.acquire_reducer()?);
        self.app.begin_primary_publication();
        Ok(())
    }

    /// The primary scan's map was published, or could not be: the scan is over for the reader.
    fn complete_primary_scan(&mut self) -> Result<(), AppError> {
        if !self.generation_rebuild_active {
            self.scan_active = false;
        }
        let reduction = self.primary_reduction.take().ok_or_else(|| {
            AppError::Invariant("a published scan had no reduction lease".to_string())
        })?;
        self.workers()?
            .finish_coordinated_work(reduction, WorkCompletion::Succeeded)?;
        self.app.start_ui();
        crate::test_events::scan_complete(self.summary.scanned_entries);
        self.animation.schedule_completion();
        #[cfg(feature = "internal")]
        self.probe_scan_complete();
        self.record_scan_store_summary();
        self.start_pending_generation_rebuild()?;
        Ok(())
    }

    fn finish_generation_rebuild(&mut self, cancelled: bool) -> Result<(), AppError> {
        if cancelled {
            return self.complete_generation_rebuild(true, None);
        }
        // As for the primary scan, the rebuild is over for the reader once its replacement map
        // was published: `complete_generation_rebuild` runs when that publication ends.
        self.app.begin_rebuild_publication();
        Ok(())
    }

    /// A rebuild ended: cancelled, or with its replacement map published (or not).
    fn complete_generation_rebuild(
        &mut self,
        cancelled: bool,
        publication_failure: Option<String>,
    ) -> Result<(), AppError> {
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
            self.app.finish_generation_rebuild(publication_failure)?;
        }
        self.record_scan_store_summary();
        self.start_pending_generation_rebuild()?;
        Ok(())
    }

    fn record_scan_store_summary(&mut self) {
        let (used, limit) = self.app.scan_store_stats();
        self.summary.scan_store_bytes = used;
        self.summary.scan_store_limit_bytes = limit;
    }

    /// Applies what the store thread reported, and finishes the publications that ended. Returns
    /// whether there was anything to do.
    fn process_scan_store_events(&mut self) -> Result<bool, AppError> {
        #[cfg(feature = "internal")]
        let publication = self
            .app
            .primary_publication_pending()
            .then(|| self.probe_phase(OwnerPhase::Publication))
            .flatten();
        let applied = self.app.process_scan_store_events();
        #[cfg(feature = "internal")]
        if let Some(mut timer) = publication
            && !self.app.primary_publication_finished()
        {
            timer.cancel();
        }
        let mut did_work = applied;
        did_work |= self.app.apply_waiting_provisional_page();
        did_work |= self.app.refresh_publication_progress();
        while let Some(finished) = self.app.take_finished_publication() {
            did_work = true;
            match finished {
                FinishedPublication::Primary => self.complete_primary_scan()?,
                FinishedPublication::Rebuild { failure } => {
                    self.complete_generation_rebuild(false, failure)?;
                }
                FinishedPublication::RebuildCancelled => {
                    self.complete_generation_rebuild(true, None)?;
                }
            }
        }
        if did_work {
            self.start_work_that_waited_for_the_store()?;
        }
        Ok(did_work)
    }

    /// Starts what only a publication's end allowed. A rebuild a deletion required waits for
    /// every publication to end; so does the next queued deletion, which starts once the map
    /// without the last one's target has arrived; and a confirmed quit that waited for a rebuild
    /// can finish only after the rebuild's own publication.
    fn start_work_that_waited_for_the_store(&mut self) -> Result<(), AppError> {
        self.start_pending_generation_rebuild()?;
        if !self.exit_after_work {
            self.start_next_deletion_planning()?;
            self.start_next_deletion_execution()?;
        }
        self.finish_exit_after_work();
        Ok(())
    }

    #[allow(clippy::too_many_lines)]
    fn handle_worker_event(&mut self, event: WorkerEvent) -> Result<(), AppError> {
        #[cfg(feature = "internal")]
        let _worker_event = self.probe_worker_event(&event);
        // Scan progress, completion, and deletion events are state changes that
        // re-arm the selected tile's sheen for one more cycle (F3).
        self.app.rearm_sheen(self.clock.now());
        let exit_work_may_change = matches!(
            &event,
            WorkerEvent::ScanBatch { .. }
                | WorkerEvent::ScanUnscanned { .. }
                | WorkerEvent::DeletionPlanned { .. }
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
                    #[cfg(feature = "internal")]
                    let _admission = self.probe_admission(input_runs.len());
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
                if is_scan_store_capacity_failure(&message) {
                    let message = safe_display_text(&message);
                    if self
                        .app
                        .note_scan_store_capacity_failure_from_worker(message.clone())
                    {
                        self.summary.unscanned_entries =
                            self.summary.unscanned_entries.saturating_add(1);
                        self.summary.last_unscanned_path =
                            path.as_deref().map(safe_display_path_text);
                        self.summary.last_unscanned_reason = Some(message.clone());
                        self.summary.last_worker_error = Some(message);
                    }
                } else {
                    self.handle_scan_failure(
                        path.as_deref(),
                        &message,
                        self.generation_rebuild_active,
                    );
                }
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
                crate::test_events::deletion_finished(
                    report.deleted_entries(),
                    report.failed_entries(),
                );
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
        if !self.exit_after_work {
            self.start_next_deletion_planning()?;
            self.start_next_deletion_execution()?;
        }
        if exit_work_may_change
            && !self.exit_after_work
            && matches!(self.app.ui_mode, crate::UiMode::Exiting { .. })
        {
            self.app.prompt_exit(self.exit_work());
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

        if self.scan_active && now >= self.next_loading_frame {
            if self.scan_view_dirty && self.app.refresh_board_from_scan() {
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
                    progress
                        .completed()
                        .load(std::sync::atomic::Ordering::Acquire),
                    progress.has_started_mutation(),
                ) {
                    self.app.mark_dirty();
                    processed = true;
                    // Deletion progress is a state change that re-arms the
                    // selected tile's sheen for one more cycle (F3).
                    self.app.rearm_sheen(now);
                }
                self.next_deletion_progress_frame = now.saturating_add(DELETION_PROGRESS_INTERVAL);
            }
        } else {
            self.last_deletion_progress = None;
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

    /// Renders a scheduled visual frame whether or not scan work kept this loop busy.
    fn render_due_frame(&mut self) -> Result<bool, AppError> {
        self.update_animation_frame();
        self.render()
    }

    /// Draws the next frame when dirty, unless a real terminal is still draining the previous
    /// one (`frame_sink`): coalesces by skipping this attempt rather than blocking on the write,
    /// so scan ingestion and input keep running on a slow terminal. `self.app.dirty` stays set,
    /// so the next opportunity renders whatever changed meanwhile instead of this moment's state.
    ///
    /// Animation and scan-progress ticks are unbounded in count, so coalescing them to at most
    /// one frame in flight is what keeps a slow terminal from falling further and further behind.
    /// A keystroke is different: discrete, bounded by how fast a person (or the harness) can
    /// produce them, and the input-latency budget applies even while a large scan-driven frame is
    /// still draining. `render_reflecting_input` is for that case; it never coalesces this way.
    fn render(&mut self) -> Result<bool, AppError> {
        self.render_with_backlog_gate(true)
    }

    /// Draws the next frame reflecting input just processed, without waiting for a still-draining
    /// frame: see `render`'s doc for why input does not coalesce behind scan or animation output.
    /// Its bytes still queue strictly after whatever is already draining or queued (`FrameSink`
    /// never reorders or drops what it is handed), so this cannot show the input out of order.
    fn render_reflecting_input(&mut self) -> Result<bool, AppError> {
        self.render_with_backlog_gate(false)
    }

    fn render_with_backlog_gate(&mut self, respect_backlog_gate: bool) -> Result<bool, AppError> {
        if let Some(sink) = &self.frame_sink
            && let Some(error) = sink.take_failure()
        {
            return Err(AppError::terminal("draw", error));
        }
        #[cfg(feature = "internal")]
        let mut frame = self.probe_phase(OwnerPhase::Render);
        let gated = respect_backlog_gate
            && self
                .frame_sink
                .as_ref()
                .is_some_and(|sink| !sink.previous_frame_drained());
        let result = if gated {
            Ok(false)
        } else {
            self.app.render_if_dirty(
                &mut self.animation,
                self.clock.now(),
                self.settings.theme.attribution().name,
                crate::theme::Theme::for_id(self.settings.theme),
                self.settings.ascii,
                self.settings.monochrome,
                self.settings.reduced_motion,
            )
        };
        if matches!(&result, Ok(true)) {
            crate::test_events::frame();
        }
        #[cfg(feature = "internal")]
        if !matches!(&result, Ok(true))
            && let Some(frame) = frame.as_mut()
        {
            frame.cancel();
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

    /// Work that ends by itself and that the loop keeps polling for: the scan, a deletion, and a
    /// publication the store thread still owes. A publication outlives the scan that began it, so
    /// the loop must not idle through one.
    fn background_work_pending(&self) -> bool {
        self.scan_active
            || self.app.deletion_work.has_background_activity()
            || self.app.scan_store_busy()
            || self
                .workers
                .as_ref()
                .is_some_and(WorkerPool::history_export_running)
    }

    fn next_timeout(&self) -> Duration {
        let now = self.clock.now();
        let mut timeout = if self.scan_active || self.app.scan_store_busy() {
            SCAN_POLL_INTERVAL
        } else if self.background_work_pending() {
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
        // A frame is waiting to draw but gated on the terminal writer draining the previous one
        // (`render`): recheck soon rather than sleeping for up to `IDLE_INPUT_WAIT`, so output
        // catching up after the scan completes still renders promptly instead of waiting for the
        // next keypress. `dirty` cannot stay set here for any other reason: an unguarded `render`
        // this same loop iteration would already have cleared it.
        if self.app.is_dirty() {
            timeout = timeout.min(WORKER_POLL_INTERVAL);
        }
        if self.stop_signals.is_some() {
            timeout = timeout.min(SIGNAL_POLL_INTERVAL);
        }
        timeout
    }

    fn wait_for_quiescence(&mut self) -> Result<(), AppError> {
        'quiescence: loop {
            while self.background_work_pending() {
                if self.process_pending_scan_entry() {
                    self.render_due_frame()?;
                    continue;
                }
                if self.process_scan_store_events()? {
                    self.render_due_frame()?;
                    continue;
                }
                if self.process_history_export()? {
                    self.render_due_frame()?;
                    continue;
                }
                if !self.app.scan_store_has_room_for_batch() {
                    std::thread::sleep(WORKER_POLL_INTERVAL);
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
                if self.background_work_pending() {
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
                self.process_deadlines();
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

/// The one place left that can only compare strings: a worker thread's sealing failure
/// crosses the event channel as a plain `message`, already classified by
/// [`scan_store_capacity_message`](crate::scan_store::session::scan_store_capacity_message) at
/// its origin, while the error was still typed. The session's own tracked quota keeps its
/// detailed wording (exact byte counts); a raw out-of-space error is replaced with the
/// canonical phrase there. Both satisfy this check, so it recognizes either, not guessed
/// OS-specific wording.
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
    scan_headless_with_stop_signals(settings, None)
}

/// Like [`scan_headless`], but ends the scan as a confirmed quit the moment a signal or
/// console-event request arrives, exactly like the interactive owner loop minus the terminal
/// and deletion UI neither has here: the run reports [`OperationOutcome::Cancelled`]. Records
/// this run's scan-store session directory ([`signals::record_headless_session_root`]) so that
/// a second request, which this scan loop and `cli::run_headless`'s own later check cannot
/// always wait for (publication and building the report have no check of their own in between),
/// still removes it and exits at once; otherwise session storage is removed the same way any
/// other headless exit removes it, by every owned value (the `ScanStore`, its
/// `ScanStoreStorage` session) dropping normally as this function returns. `cli::run_main` is
/// the only caller that passes one.
pub(crate) fn scan_headless_with_stop_signals(
    settings: RuntimeSettings,
    stop_signals: Option<Receiver<StopRequest>>,
) -> Result<OperationOutcome<ScanReport>, AppError> {
    let scan_store_session = ScanStoreStorage::new_for_scan_store_mib(
        settings.scan_store_mib,
        settings.scan_store_reserve_mib,
        settings.scan_store_dir.as_deref(),
    )
    .map_err(|error| AppError::Config(error.to_string()))?;
    if stop_signals.is_some() {
        signals::record_headless_session_root(scan_store_session.root().to_path_buf());
    }
    scan_headless_with_scan_store_session(settings, scan_store_session, stop_signals)
}

#[cfg(test)]
fn scan_headless_with_scan_store_storage(
    settings: RuntimeSettings,
    scan_store_storage: TemporaryStorage,
) -> Result<OperationOutcome<ScanReport>, AppError> {
    let scan_store_session =
        ScanStoreStorage::new(scan_store_storage, settings.scan_store_dir.as_deref())
            .map_err(|error| AppError::Config(error.to_string()))?;
    scan_headless_with_scan_store_session(settings, scan_store_session, None)
}

#[allow(clippy::too_many_lines, clippy::needless_pass_by_value)]
fn scan_headless_with_scan_store_session(
    settings: RuntimeSettings,
    scan_store_session: ScanStoreStorage,
    stop_signals: Option<Receiver<StopRequest>>,
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
            let event = match &stop_signals {
                Some(stop_signals) => crossbeam_channel::select! {
                    recv(workers.events()) -> event => event.map_err(|_| {
                        AppError::Worker("scanner event channel disconnected".to_string())
                    })?,
                    recv(stop_signals) -> _ => {
                        // A signal headless run has no terminal or deletion UI to settle
                        // first: ending the scan here is the whole of the confirmed quit.
                        // Session storage is removed the same way any other headless exit
                        // removes it, by every owned value dropping normally as this function
                        // returns below (`scan_store`, its `ScanStoreStorage` session); a
                        // second request only confirms what the first already decided.
                        return Ok(true);
                    },
                },
                None => workers.events().recv().map_err(|_| {
                    AppError::Worker("scanner event channel disconnected".to_string())
                })?,
            };
            match event {
                WorkerEvent::ScanBatch {
                    lease,
                    entries,
                    input_runs,
                } => {
                    // This loop admits what it receives, so the batch's in-flight credit comes
                    // back as the batch arrives: that is how the headless scan has always paced
                    // the scanner, and how long admission takes does not change it.
                    let input_runs = input_runs.into_runs_returning_credit();
                    if !scan_store_capacity_exhausted && !input_runs.is_empty() {
                        let lease = lease.as_ref().ok_or_else(|| {
                            AppError::Invariant(
                                "scanner emitted an unleased sealed batch".to_string(),
                            )
                        })?;
                        for run in input_runs {
                            if let Err(error) = scan_store.accept_leased_input_run(lease, run) {
                                if !is_scan_store_capacity_error(&error) {
                                    return Err(AppError::Model(error.to_string()));
                                }
                                let message = scan_store_capacity_message(&error);
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
                    let input_runs = input_runs.into_runs_returning_credit();
                    let represented = !input_runs.is_empty();
                    if !scan_store_capacity_exhausted && represented {
                        let lease = lease.as_ref().ok_or_else(|| {
                            AppError::Invariant(
                                "scanner emitted an unleased sealed coverage result".to_string(),
                            )
                        })?;
                        for run in input_runs {
                            if let Err(error) = scan_store.accept_leased_input_run(lease, run) {
                                if !is_scan_store_capacity_error(&error) {
                                    return Err(AppError::Model(error.to_string()));
                                }
                                let message = scan_store_capacity_message(&error);
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
                        if let Some(path) = path
                            .as_deref()
                            .and_then(|path| path.strip_prefix(&settings.root).ok())
                            .and_then(|relative| RelativePath::from_path(relative).ok())
                        {
                            scan_store.record_unreadable_directory(path);
                        }
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
    // The scan loop's own select! notices a request that arrives while it is still running; a
    // request that lands in the narrow gap between the loop ending and here (nothing polls for
    // one in between) still ends the run as a cancelled, 130 exit, exactly like one the loop
    // itself caught - checked once more, non-blocking, right before the two phases ahead
    // (publication, then building the report) that have no check of their own. `cli::run_headless`
    // checks once more again, after both, for the same reason.
    let cancelled = cancelled
        || stop_signals
            .as_ref()
            .is_some_and(|stop_signals| stop_signals.try_recv().is_ok());
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
    next_export_path_in(&directory, kind)
}

/// The first of `excise-{kind}.json`, `excise-{kind}-1.json`, and so on that is free in
/// `directory`.
fn next_export_path_in(directory: &Path, kind: &str) -> Result<PathBuf, String> {
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

/// Buffers `writer`, writes an export through `write`, and flushes explicitly, turning any
/// failure (including a flush failure on the writer's final buffered bytes) into the plain
/// message both export notices report. Shared by both TUI exports: a `BufWriter` dropped
/// without an explicit flush discards that failure, so a full disk would silently truncate an
/// export with no failure notice at all.
fn export_report<W: Write>(
    writer: W,
    write: impl FnOnce(&mut BufWriter<W>) -> Result<(), ReportError>,
) -> Result<(), String> {
    write_buffered(writer, write).map_err(|error| error.to_string())
}

#[must_use]
pub const fn outcome_exit_class(outcome: &OperationOutcome<RunSummary>) -> ExitClass {
    outcome.exit_class()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;
    use std::sync::{Arc, Mutex, mpsc};

    use crate::tests::cases::test_utils::test_backend_factory;
    use crate::tests::fakes::{TerminalEvents, TestBackend as LoggingBackend};

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

    /// Delivers exactly one real terminal key, then goes idle. Used to prove
    /// that input re-arms the selected tile's sheen (F3).
    struct OnceKeyInput {
        delivered: bool,
    }

    impl InputSource for OnceKeyInput {
        fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
            Ok(!self.delivered)
        }

        fn read(&mut self) -> Result<InputEvent, AppError> {
            assert!(!self.delivered, "this fake delivers exactly one keypress");
            self.delivered = true;
            Ok(InputEvent::Terminal(Event::Key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ),
            )))
        }
    }

    /// Hands out its keys only to a poll that waits, the way a key typed while the loop sleeps in
    /// its idle poll reaches it, and notes how many frames had been drawn each time one is read.
    struct KeysWhileIdle {
        keys: VecDeque<Event>,
        frames: Arc<Mutex<Vec<String>>>,
        frames_at_read: Rc<RefCell<Vec<usize>>>,
        started: Instant,
    }

    impl KeysWhileIdle {
        fn new(
            keys: impl IntoIterator<Item = Event>,
            frames: &Arc<Mutex<Vec<String>>>,
            frames_at_read: &Rc<RefCell<Vec<usize>>>,
        ) -> Self {
            Self {
                keys: keys.into_iter().collect(),
                frames: Arc::clone(frames),
                frames_at_read: Rc::clone(frames_at_read),
                started: Instant::now(),
            }
        }
    }

    impl InputSource for KeysWhileIdle {
        fn poll(&mut self, timeout: Duration) -> Result<bool, AppError> {
            // Gives up on a loop that never reaches its idle poll, which would spin here for good.
            if self.started.elapsed() > Duration::from_secs(30) {
                return Err(AppError::Invariant(
                    "the loop never waited for input".to_string(),
                ));
            }
            Ok(!timeout.is_zero() && !self.keys.is_empty())
        }

        fn read(&mut self) -> Result<InputEvent, AppError> {
            self.frames_at_read
                .borrow_mut()
                .push(logged_frames(&self.frames));
            self.keys
                .pop_front()
                .map(InputEvent::Terminal)
                .ok_or_else(|| AppError::Invariant("no key was left to read".to_string()))
        }
    }

    fn key(character: char) -> Event {
        Event::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char(character),
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    /// A terminal writer that blocks on the bytes it is given until the sending half of
    /// `stalled` drops, as a terminal that has stopped reading does.
    struct StalledWriter {
        stalled: mpsc::Receiver<()>,
    }

    impl io::Write for StalledWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            // Nothing is ever sent: this returns once the sender drops.
            let _ = self.stalled.recv();
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A frame sink whose writer is stuck on the frame it was handed, so its render gate is shut
    /// until the returned sender drops.
    fn undrained_frame_sink() -> (FrameSink, mpsc::Sender<()>) {
        let (release, stalled) = mpsc::channel();
        let (_writer, sink) =
            crate::terminal::spawn_frame_writer(StalledWriter { stalled }, io::sink())
                .expect("the terminal writer thread should spawn");
        sink.enqueue(b"the previous frame".to_vec())
            .expect("the writer should accept a frame");
        assert!(
            !sink.previous_frame_drained(),
            "the writer should still be busy with that frame"
        );
        (sink, release)
    }

    /// How many frames the logging backend has recorded.
    fn logged_frames(frames: &Arc<Mutex<Vec<String>>>) -> usize {
        frames.lock().expect("the frame log should lock").len()
    }

    /// An owner loop over a backend that logs its frames, whose terminal writer is still busy with
    /// the last one: the render gate holds back every frame but a key's until the writer is
    /// released, which is when this drops.
    struct BusyTerminal {
        owner: OwnerLoop<LoggingBackend>,
        frames: Arc<Mutex<Vec<String>>>,
        _writer_release: mpsc::Sender<()>,
    }

    impl BusyTerminal {
        fn new(root: &Path) -> Self {
            let (_, frames, backend) = test_backend_factory(80, 24);
            let mut owner = owner_with_workers(backend, root, None);
            // The backend leaves its first frame out of the log, so that one is drawn before any
            // frame is counted.
            owner.render().expect("the first frame should draw");
            let (sink, writer_release) = undrained_frame_sink();
            owner.frame_sink = Some(sink);
            Self {
                owner,
                frames,
                _writer_release: writer_release,
            }
        }

        fn frames_drawn(&self) -> usize {
            logged_frames(&self.frames)
        }
    }

    fn shut_down_workers<B: Backend>(owner: &mut OwnerLoop<B>) {
        owner
            .workers
            .take()
            .expect("workers should still be owned")
            .shutdown()
            .expect("workers should shut down cleanly");
    }

    /// The coordinator's active leases, and how much work it has recorded as succeeded.
    fn scheduler_counts<B: Backend>(owner: &OwnerLoop<B>) -> (usize, u64) {
        let snapshot = owner
            .workers
            .as_ref()
            .expect("workers should still be owned")
            .scheduler_snapshot()
            .expect("the coordinator should answer");
        (snapshot.active_leases, snapshot.terminal.succeeded())
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
            frame_sink: None,
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
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
        let event_left_queued = !owner
            .workers
            .as_ref()
            .expect("workers should still be owned")
            .events()
            .is_empty();
        owner
            .workers
            .take()
            .expect("workers should still be owned")
            .shutdown()
            .expect("workers should shut down");

        assert!(!processed, "queued input must preempt scan work");
        assert_eq!(scanned_entries, 0);
        assert!(
            event_left_queued,
            "the scanner's event must wait for the input instead of being consumed"
        );
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
            frame_sink: None,
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
        };

        owner
            .handle_worker_event(WorkerEvent::ScanBatch {
                lease: None,
                entries: (0..=MAX_SCAN_ENTRIES_PER_SLICE)
                    .map(|index| ScannedEntry {
                        metadata: scanner::EntryMetadata::from_std(&first_metadata),
                        path: root.path().join(format!("entry-{index}")),
                        identity: first_identity.clone(),
                    })
                    .collect(),
                input_runs: SealedBatch::empty(),
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
            metadata: scanner::EntryMetadata::from_std(&entry_metadata),
            path: entry.clone(),
            identity: entry_identity,
        }]);
        let mut owner = OwnerLoop {
            app,
            input: Box::new(IdleInput),
            workers: None,
            clock: Box::new(clock),
            animation,
            frame_sink: None,
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
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
    #[cfg(any(target_os = "linux", windows))]
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
            frame_sink: None,
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
        }
    }

    /// Builds an `App` with one real, selectable entry and a `FileToDelete` for it, exactly as
    /// other owner-loop deletion tests do (`append_scan_store_entry_for_test`, `finalize_scan`,
    /// `start_ui`, `request_deletion`).
    #[cfg(any(unix, windows))]
    fn app_with_one_deletable_entry(
        root: &std::path::Path,
        root_identity: &NativeIdentity,
    ) -> (App<TestBackend>, crate::state::FileToDelete) {
        let entry = root.join("entry");
        std::fs::write(&entry, b"x").expect("test entry should be created");
        let entry_metadata =
            std::fs::symlink_metadata(&entry).expect("test entry metadata should exist");
        let entry_identity = crate::native_path::identity_for(&entry, &entry_metadata)
            .expect("test entry identity should be readable")
            .expect("test entry should not be a link");
        let mut app = App::new_with_root_identity(
            TestBackend::new(80, 24),
            root.to_path_buf(),
            root_identity.clone(),
            false,
            false,
            crate::model::DEFAULT_PROCESS_MIB,
            KeyPreset::Vim,
            None,
            false,
        )
        .expect("app should initialize");
        app.append_scan_store_entry_for_test(&entry_metadata, &entry, &entry_identity);
        app.finalize_scan();
        app.start_ui();
        let mut animation = AnimationScheduler::new(true, false, Duration::ZERO);
        app.render_if_dirty(
            &mut animation,
            Duration::ZERO,
            "test",
            crate::theme::Theme::for_id(ThemeId::ExciseDark),
            false,
            false,
            true,
        )
        .expect("entry should render into the map");
        let target = app
            .request_deletion()
            .expect("rendered target should delete");
        (app, target)
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_graceful_stop_cancels_a_pending_deletion_plan() {
        let root = tempfile::tempdir().expect("test root should be created");
        let root_metadata =
            std::fs::symlink_metadata(root.path()).expect("test root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &root_metadata)
            .expect("test root identity should be readable")
            .expect("test root should not be a link");
        let (mut app, target) = app_with_one_deletable_entry(root.path(), &root_identity);
        assert!(app.queue_deletion_confirmation(target.clone(), false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        let (work_id, confirmed) = app
            .arm_and_confirm_deletion_target()
            .expect("confirmation should arm deletion work");
        assert!(app.queue_confirmed_deletion(work_id, confirmed, Duration::ZERO));
        assert_eq!(
            app.deletion_work.pending_count(),
            1,
            "the plan must be pending, not yet executing, before the stop"
        );

        let mut owner = owner_for_completed_deletion(app, root.path(), root_identity);

        owner
            .begin_signal_quit()
            .expect("a graceful stop should always succeed");

        assert!(owner.signal_quit);
        assert_eq!(
            owner.app.deletion_work.pending_count(),
            0,
            "the pending plan must be cancelled"
        );
        assert!(
            !owner.app.deletion_work.has_work(),
            "nothing is left to wait on, so the run can end at once"
        );
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn a_graceful_stop_refuses_the_active_deletions_next_entry() {
        let root = tempfile::tempdir().expect("test root should be created");
        let root_metadata =
            std::fs::symlink_metadata(root.path()).expect("test root metadata should exist");
        let root_identity = crate::native_path::identity_for(root.path(), &root_metadata)
            .expect("test root identity should be readable")
            .expect("test root should not be a link");
        let (mut app, target) = app_with_one_deletable_entry(root.path(), &root_identity);
        assert!(app.queue_deletion_confirmation(target.clone(), false, 1024, Duration::ZERO));
        assert!(app.show_next_deletion_confirmation());
        let (work_id, confirmed) = app
            .arm_and_confirm_deletion_target()
            .expect("confirmation should arm deletion work");
        assert!(app.queue_confirmed_deletion(work_id, confirmed, Duration::ZERO));
        // Jumps straight to the executing stage a real plan and worker round-trip would
        // eventually reach. What this test checks is the owner loop's own request, not the
        // executor's response to it: `deletion.rs`'s `soft_cancelled` tests already cover the
        // executor honoring the same flag this asserts was set.
        app.deletion_work.set_execution_for_test(work_id, 1);
        assert!(app.deletion_work.active_progress().is_some());

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
        assert!(!workers.deletion_soft_cancelled_for_test());
        let mut owner = owner_for_completed_deletion(app, root.path(), root_identity);
        owner.workers = Some(workers);

        owner
            .begin_signal_quit()
            .expect("a graceful stop should always succeed");

        assert!(owner.signal_quit);
        assert!(
            matches!(
                owner.app.ui_mode,
                crate::UiMode::Exiting {
                    work: ExitWork::Stopping { .. },
                    ..
                }
            ),
            "an active deletion must be asked to stop, not silently cancelled"
        );
        assert!(
            owner
                .workers
                .as_ref()
                .expect("workers should still be owned")
                .deletion_soft_cancelled_for_test(),
            "the owner loop must set the same stop flag the executor's entry-boundary check \
             reads, so it is refused its next entry rather than silently left running"
        );

        owner
            .workers
            .take()
            .expect("workers should still be owned")
            .shutdown()
            .expect("workers should shut down cleanly");
    }

    /// An owner loop over a running worker pool that is scanning `root`, with idle input and no
    /// frame gate: tests replace the parts they exercise.
    fn owner_with_workers<B: Backend>(
        backend: B,
        root: &std::path::Path,
        stop_signals: Option<Receiver<StopRequest>>,
    ) -> OwnerLoop<B> {
        let root_metadata =
            std::fs::symlink_metadata(root).expect("test root metadata should exist");
        let root_identity = crate::native_path::identity_for(root, &root_metadata)
            .expect("test root identity should be readable")
            .expect("test root should not be a link");
        let app = App::new_with_root_identity(
            backend,
            root.to_path_buf(),
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
                root: root.to_path_buf(),
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
        OwnerLoop {
            app,
            input: Box::new(IdleInput),
            workers: Some(workers),
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(true, true, Duration::ZERO),
            frame_sink: None,
            settings: RuntimeSettings {
                root: root.to_path_buf(),
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals,
            signal_quit: false,
            forced_stop: false,
        }
    }

    #[test]
    fn a_signal_quit_run_ends_with_the_interrupted_class() {
        let root = tempfile::tempdir().expect("test root should be created");
        let (sender, receiver) = crossbeam_channel::unbounded();
        sender
            .send(StopRequest::Graceful)
            .expect("the channel should accept the queued request");
        let mut owner = owner_with_workers(TestBackend::new(80, 24), root.path(), Some(receiver));

        let outcome = owner.run_loop().expect("the loop should end cleanly");
        owner
            .workers
            .take()
            .expect("workers should still be owned")
            .shutdown()
            .expect("workers should shut down cleanly");

        assert!(
            matches!(outcome, OperationOutcome::Cancelled { .. }),
            "a signal-driven quit must report Cancelled regardless of scan state: {outcome:?}"
        );
        assert_eq!(outcome.exit_class(), ExitClass::Interrupted);
    }

    #[test]
    fn a_second_stop_forces_the_exit_past_a_gate_the_first_alone_would_wait_on() {
        let root = tempfile::tempdir().expect("test root should be created");
        let (sender, receiver) = crossbeam_channel::unbounded();
        sender
            .send(StopRequest::Graceful)
            .expect("the channel should accept the queued request");
        sender
            .send(StopRequest::Forced)
            .expect("the channel should accept the queued request");
        let mut owner = owner_with_workers(TestBackend::new(80, 24), root.path(), Some(receiver));
        // Simulates a rescan the first request's graceful handling would otherwise wait out
        // (a partial deletion schedules one; nothing in this test ever resolves it), so only
        // the second, forced request can end the run.
        owner.generation_rebuild_active = true;
        owner.generation_rebuild_target = Some(root.path().to_path_buf());

        assert!(
            owner
                .process_stop_signals()
                .expect("draining the queued requests should succeed")
        );

        assert!(owner.signal_quit);
        assert!(
            !owner.app.is_running,
            "the forced request must end the run even though the rebuild gate never cleared \
             on its own, unlike the first request alone"
        );

        owner
            .workers
            .take()
            .expect("workers should still be owned")
            .shutdown()
            .expect("workers should shut down cleanly");
    }

    /// Idle input that sleeps through its poll, so a loop waiting on a signal does not spin.
    struct NapInput;

    impl InputSource for NapInput {
        fn poll(&mut self, timeout: Duration) -> Result<bool, AppError> {
            std::thread::sleep(timeout.min(Duration::from_millis(5)));
            Ok(false)
        }

        fn read(&mut self) -> Result<InputEvent, AppError> {
            panic!("idle input must not be read")
        }
    }

    /// An owner loop over a settled scan of `root`, run to its end on a thread of its own. A test
    /// then tells a run that never ends from one that ends late, and sends it stop requests while
    /// it waits. `prepare` runs on that thread after the scan settled, and what it returns lives
    /// until the run is over; the thread reports `prepared` once `prepare` has run, and the exit
    /// class (or the error) when the run returns.
    struct OwnerApart {
        prepared: mpsc::Receiver<()>,
        ended: mpsc::Receiver<Result<ExitClass, String>>,
        thread: std::thread::JoinHandle<()>,
    }

    fn run_owner_apart<G: 'static>(
        root: &std::path::Path,
        stops: Receiver<StopRequest>,
        prepare: impl FnOnce(&mut OwnerLoop<TestBackend>) -> G + Send + 'static,
    ) -> OwnerApart {
        let root = root.to_path_buf();
        let (prepared_sender, prepared) = mpsc::channel();
        let (ended_sender, ended) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut owner = owner_with_workers(TestBackend::new(80, 24), &root, Some(stops));
            owner.input = Box::new(NapInput);
            owner
                .app
                .start_scan_store_thread()
                .expect("the store thread should start");
            owner.wait_for_quiescence().expect("the scan should settle");
            let kept = prepare(&mut owner);
            let _ = prepared_sender.send(());
            let result = owner
                .run()
                .map(|outcome| outcome.exit_class())
                .map_err(|error| error.to_string());
            let _ = ended_sender.send(result);
            drop(kept);
        });
        OwnerApart {
            prepared,
            ended,
            thread,
        }
    }

    /// The exit class a run that was told to stop ended with, within the time a stuck thread
    /// that never ends would otherwise cost it all of.
    fn forced_run_class(apart: &OwnerApart) -> ExitClass {
        apart
            .ended
            .recv_timeout(Duration::from_secs(5))
            .expect("a forced stop must end the run although the thread it waits on never ends")
            .expect("the run should end without an error")
    }

    /// A second signal while the shutdown waits on the store thread, which is stuck inside a call
    /// (a hung file system), stops the wait. Nothing else can: the terminal is still the
    /// owner's to restore, and the owner is the one waiting.
    #[test]
    fn a_forced_stop_ends_a_shutdown_waiting_on_a_stuck_store_thread() {
        let root = tempfile::tempdir().expect("test root should be created");
        let (stops, receiver) = crossbeam_channel::unbounded();
        let apart = run_owner_apart(root.path(), receiver, |owner| {
            owner.app.hold_scan_store_thread_for_test()
        });
        apart
            .prepared
            .recv_timeout(Duration::from_secs(10))
            .expect("the store thread should be held");

        stops
            .send(StopRequest::Graceful)
            .expect("the first stop request should be accepted");
        assert!(
            apart
                .ended
                .recv_timeout(Duration::from_millis(500))
                .is_err(),
            "a quit waits for the store thread, which is stuck"
        );
        stops
            .send(StopRequest::Forced)
            .expect("the second stop request should be accepted");

        assert_eq!(forced_run_class(&apart), ExitClass::Interrupted);
        apart.thread.join().expect("the run's thread should end");
    }

    /// The loop can take the second request itself, before the shutdown starts waiting. No
    /// signal is left to arrive then, so the request taken stays the answer to the wait.
    #[test]
    fn a_forced_stop_the_loop_already_took_still_ends_the_wait_on_a_stuck_store_thread() {
        let root = tempfile::tempdir().expect("test root should be created");
        let (stops, receiver) = crossbeam_channel::unbounded();
        stops
            .send(StopRequest::Graceful)
            .expect("the first stop request should be accepted");
        stops
            .send(StopRequest::Forced)
            .expect("the second stop request should be accepted");
        let apart = run_owner_apart(root.path(), receiver, |owner| {
            owner.app.hold_scan_store_thread_for_test()
        });

        assert_eq!(forced_run_class(&apart), ExitClass::Interrupted);
        apart.thread.join().expect("the run's thread should end");
    }

    /// As for the store thread, for the deletion-history export, which writes into the folder the
    /// program was started in, however slow its file system.
    #[test]
    fn a_forced_stop_ends_a_shutdown_waiting_on_a_stuck_history_export() {
        let root = tempfile::tempdir().expect("test root should be created");
        let (stops, receiver) = crossbeam_channel::unbounded();
        let apart = run_owner_apart(root.path(), receiver, |owner| {
            let (entered_sender, entered) = mpsc::channel();
            let (release, released) = mpsc::channel::<()>();
            owner
                .workers
                .as_ref()
                .expect("workers should be running")
                .start_history_export_with(1, move |_| {
                    let _ = entered_sender.send(());
                    // Returns when the test drops its end.
                    let _ = released.recv();
                    Err("the export was held".to_string())
                })
                .expect("the export should start");
            entered
                .recv_timeout(Duration::from_secs(10))
                .expect("the export should be inside its write");
            release
        });
        apart
            .prepared
            .recv_timeout(Duration::from_secs(10))
            .expect("the export should be held");

        stops
            .send(StopRequest::Graceful)
            .expect("the first stop request should be accepted");
        assert!(
            apart
                .ended
                .recv_timeout(Duration::from_millis(500))
                .is_err(),
            "a quit waits for the export, which is stuck"
        );
        stops
            .send(StopRequest::Forced)
            .expect("the second stop request should be accepted");

        assert_eq!(forced_run_class(&apart), ExitClass::Interrupted);
        apart.thread.join().expect("the run's thread should end");
    }

    /// A run whose deletion executor and store thread are both held inside a call that does not
    /// return: the executor inside an entry, the store thread inside a write. The sender returned
    /// lets the entry go.
    fn run_with_the_executor_and_the_store_thread_held(
        root: &std::path::Path,
        stops: Receiver<StopRequest>,
    ) -> (OwnerApart, crossbeam_channel::Sender<()>) {
        let (release_entry, entry_released) = crossbeam_channel::bounded::<()>(0);
        let apart = run_owner_apart(root, stops, move |owner| {
            owner
                .workers
                .as_ref()
                .expect("workers should be running")
                .hold_executor_for_test(entry_released);
            owner.app.hold_scan_store_thread_for_test()
        });
        apart
            .prepared
            .recv_timeout(Duration::from_secs(10))
            .expect("the executor and the store thread should be held");
        (apart, release_entry)
    }

    /// A second signal does not end the wait for the deletion executor, however long its entry
    /// takes: an entry is moved aside before it is removed, so a process that ended inside one
    /// could leave a target that is neither intact nor gone. The run ends once the entry returns,
    /// and only then gives up on the store thread, held in the same run, after its grace.
    #[test]
    fn a_forced_stop_still_waits_for_a_deletion_executor_held_inside_an_entry() {
        let root = tempfile::tempdir().expect("test root should be created");
        let (stops, receiver) = crossbeam_channel::unbounded();
        let (apart, release_entry) =
            run_with_the_executor_and_the_store_thread_held(root.path(), receiver);

        stops
            .send(StopRequest::Graceful)
            .expect("the first stop request should be accepted");
        assert!(
            apart
                .ended
                .recv_timeout(Duration::from_millis(500))
                .is_err(),
            "a quit waits for the executor, which is inside an entry"
        );
        stops
            .send(StopRequest::Forced)
            .expect("the second stop request should be accepted");
        assert!(
            apart
                .ended
                .recv_timeout(signals::FORCED_STOP_GRACE + Duration::from_millis(500))
                .is_err(),
            "a forced stop ended the wait for a deletion executor that was inside an entry"
        );
        drop(release_entry);

        assert_eq!(forced_run_class(&apart), ExitClass::Interrupted);
        apart.thread.join().expect("the run's thread should end");
    }

    /// As above when the loop took the second request itself, before the shutdown began: the
    /// grace that request starts does not run while the executor is waited for, so the store
    /// thread is still given its full grace afterwards and not left at once.
    #[test]
    fn a_forced_stop_the_loop_already_took_still_waits_for_a_deletion_executor_held_inside_an_entry()
     {
        let root = tempfile::tempdir().expect("test root should be created");
        let (stops, receiver) = crossbeam_channel::unbounded();
        stops
            .send(StopRequest::Graceful)
            .expect("the first stop request should be accepted");
        stops
            .send(StopRequest::Forced)
            .expect("the second stop request should be accepted");
        let (apart, release_entry) =
            run_with_the_executor_and_the_store_thread_held(root.path(), receiver);

        assert!(
            apart
                .ended
                .recv_timeout(signals::FORCED_STOP_GRACE + Duration::from_millis(500))
                .is_err(),
            "a forced stop ended the wait for a deletion executor that was inside an entry"
        );
        drop(release_entry);

        assert_eq!(forced_run_class(&apart), ExitClass::Interrupted);
        apart.thread.join().expect("the run's thread should end");
    }

    /// A reader's keystroke is drawn at once, even while the terminal is still draining the last
    /// frame: the render gate coalesces the frames a scan and its animations ask for, and a key
    /// that wakes the idle poll is not one of them.
    #[test]
    fn a_key_that_wakes_the_idle_poll_is_drawn_while_the_terminal_is_still_draining() {
        let root = tempfile::tempdir().expect("test root should be created");
        let mut busy = BusyTerminal::new(root.path());
        let baseline = busy.frames_drawn();
        let frames_at_read = Rc::new(RefCell::new(Vec::new()));
        busy.owner.input = Box::new(KeysWhileIdle::new(
            [key('q'), key('y')],
            &busy.frames,
            &frames_at_read,
        ));

        busy.owner
            .run_loop()
            .expect("the loop should end at the confirmed quit");
        shut_down_workers(&mut busy.owner);

        let frames_at_read = frames_at_read.take();
        assert_eq!(frames_at_read.len(), 2, "both keys should reach the loop");
        assert_eq!(
            frames_at_read[0], baseline,
            "the gate holds back every frame the loop draws on its own"
        );
        assert!(
            frames_at_read[1] > baseline,
            "the key that woke the idle poll must be drawn before the loop waits for the next one"
        );
    }

    /// A confirmed quit draws nothing more: the terminal is restored right after, and a frame
    /// queued behind the one still draining would only delay that.
    #[test]
    fn a_confirmed_quit_read_in_an_input_batch_draws_no_further_frame() {
        let root = tempfile::tempdir().expect("test root should be created");
        let mut busy = BusyTerminal::new(root.path());
        let baseline = busy.frames_drawn();
        busy.owner.input = Box::new(TerminalEvents::new(vec![Some(key('q')), Some(key('y'))]));

        busy.owner
            .run_loop()
            .expect("the loop should end at the confirmed quit");
        shut_down_workers(&mut busy.owner);

        assert!(
            busy.owner.app.is_dirty(),
            "the quit prompt was still waiting to be drawn when the quit was confirmed"
        );
        assert_eq!(
            busy.frames_drawn(),
            baseline,
            "the confirmed quit must not queue a frame behind the draining one"
        );
    }

    /// As above, for a quit confirmed by a key that wakes the idle poll.
    #[test]
    fn a_confirmed_quit_read_by_the_idle_poll_draws_no_further_frame() {
        let root = tempfile::tempdir().expect("test root should be created");
        let mut busy = BusyTerminal::new(root.path());
        let baseline = busy.frames_drawn();
        let work = busy.owner.exit_work();
        busy.owner.app.prompt_exit(work);
        let frames_at_read = Rc::new(RefCell::new(Vec::new()));
        busy.owner.input = Box::new(KeysWhileIdle::new(
            [key('y')],
            &busy.frames,
            &frames_at_read,
        ));

        busy.owner
            .run_loop()
            .expect("the loop should end at the confirmed quit");
        shut_down_workers(&mut busy.owner);

        assert_eq!(
            frames_at_read.take(),
            vec![baseline],
            "the confirming key is the only one read, before anything was drawn"
        );
        assert!(
            busy.owner.app.is_dirty(),
            "the quit prompt was still waiting to be drawn when the quit was confirmed"
        );
        assert_eq!(
            busy.frames_drawn(),
            baseline,
            "the confirmed quit must not queue a frame behind the draining one"
        );
    }

    /// The scan is over for the reader when its map is published, not when the scanner is done:
    /// the loop holds the reduction lease across the wait, and finishes the scan once, when the
    /// publication's result is applied.
    #[test]
    fn the_scan_finishes_when_its_map_is_published_not_when_the_scanner_is_done() {
        let root = tempfile::tempdir().expect("test root should be created");
        let mut owner = owner_with_workers(TestBackend::new(80, 24), root.path(), None);
        let deadline = Instant::now() + Duration::from_secs(10);
        while owner.primary_scan_active {
            assert!(Instant::now() < deadline, "the scanner never finished");
            if !owner
                .process_worker_batch()
                .expect("the scanner's events should be applied")
            {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        assert!(
            !owner.scan_cancelled,
            "the scanner should complete its scan"
        );
        let (leases_held, succeeded_before) = scheduler_counts(&owner);
        assert!(
            leases_held >= 1,
            "the coordinator counts the reduction lease as active"
        );

        assert!(
            owner.scan_active,
            "the scan stays open for the reader until its map is published"
        );
        assert!(
            owner.primary_reduction.is_some(),
            "the reduction lease is held across the publication"
        );
        assert!(owner.app.primary_publication_pending());

        assert!(
            owner
                .process_scan_store_events()
                .expect("the publication's result should be applied")
        );

        assert!(!owner.scan_active, "the published map ends the scan");
        assert!(
            owner.primary_reduction.is_none(),
            "the reduction lease is finished with the scan"
        );
        assert!(!owner.app.scan_store_busy());
        let finished = scheduler_counts(&owner);
        assert_eq!(
            finished,
            (
                leases_held.saturating_sub(1),
                succeeded_before.saturating_add(1)
            ),
            "the coordinator records the reduction as succeeded"
        );

        assert!(
            !owner
                .process_scan_store_events()
                .expect("an ended publication has nothing left to apply"),
            "the publication finishes the scan once"
        );
        assert_eq!(scheduler_counts(&owner), finished);
        shut_down_workers(&mut owner);
    }

    /// The map without the removed target is the overlay the store thread builds, and the owner
    /// loop reflows the map around the survivor as that overlay arrives. That takes a removal
    /// the executor can show left no link behind, which for a file is Linux and Windows: macOS
    /// opens nothing it removes, and sends the map back to a scan after a removed file (see
    /// `a_deletion_of_a_file_sends_the_map_back_to_a_scan_on_macos` in the app's tests).
    #[cfg(any(target_os = "linux", windows))]
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
        // The map without the target is published by the store thread; the owner loop swaps it in
        // when the result arrives, which is what reflows the map around the survivor.
        assert!(
            owner
                .process_scan_store_events()
                .expect("the deletion's overlay map should be applied")
        );
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

    /// An owner loop with no worker pool, over an app that has published a map and is now
    /// rebuilding it: the rebuild's scan is the only work, and its end is what a test delivers.
    #[allow(clippy::too_many_lines)]
    fn rebuilding_owner_without_workers(root: &Path) -> OwnerLoop<TestBackend> {
        let root_metadata =
            std::fs::symlink_metadata(root).expect("scan rebuild root metadata should exist");
        let root_identity = crate::native_path::identity_for(root, &root_metadata)
            .expect("scan rebuild root identity should be readable")
            .expect("scan rebuild root should not be a link");
        let mut app = App::new_with_root_identity(
            TestBackend::new(80, 24),
            root.to_path_buf(),
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
        OwnerLoop {
            app,
            input: Box::new(PendingInput),
            workers: None,
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(false, false, Duration::ZERO),
            frame_sink: None,
            settings: RuntimeSettings {
                root: root.to_path_buf(),
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
            scan_view_dirty: false,
            scan_view_root,
            pending_scan_entries: VecDeque::new(),
            scan_cancelled: false,
            generation_rebuild_active: true,
            generation_rebuild_target: Some(root.to_path_buf()),
            cancelled_while_scanning: false,
            exit_after_work: false,
            timed_actions: Vec::new(),
            next_loading_frame: Duration::ZERO,
            next_deletion_progress_frame: Duration::ZERO,
            last_deletion_progress: None,
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
        }
    }

    #[test]
    fn generation_rebuild_lifecycle_does_not_schedule_header_completion() {
        let root = tempfile::tempdir().expect("scan rebuild root should exist");
        let mut owner = rebuilding_owner_without_workers(root.path());

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
        // The rebuild is over for the reader once the store thread has published its map.
        assert!(
            owner
                .process_scan_store_events()
                .expect("the rebuilt map should be applied")
        );
        assert!(
            !owner.generation_rebuild_active,
            "the publication's result completes the rebuild"
        );
        assert_eq!(
            owner.animation.pending_slots(),
            0,
            "finishing generation work must not flash completion through the header"
        );
    }

    /// A confirmed quit waits for the rebuild a deletion required. The rebuild is over only when
    /// its map is published, which is later than its scan: the quit has to finish then, because
    /// nothing else would wake the loop for it.
    #[test]
    fn a_confirmed_quit_that_waited_for_a_rebuild_exits_when_its_map_is_published() {
        let root = tempfile::tempdir().expect("scan rebuild root should exist");
        let mut owner = rebuilding_owner_without_workers(root.path());
        owner
            .cancel_pending_work_and_exit(true)
            .expect("the quit should be accepted");
        owner.finish_exit_after_work();
        assert!(owner.app.is_running, "the rebuild is still under way");

        owner
            .handle_worker_event(WorkerEvent::ScanFinished { cancelled: false })
            .expect("the rebuild's scan should end");
        assert!(
            owner.app.is_running,
            "the rebuild's map is still being published"
        );
        assert!(owner.exit_after_work);

        assert!(
            owner
                .process_scan_store_events()
                .expect("the rebuilt map should be applied")
        );

        assert!(!owner.generation_rebuild_active);
        assert!(
            !owner.app.is_running,
            "the publication ended the last work the quit waited for"
        );
        assert!(!owner.exit_after_work);
    }

    /// As above, when the rebuild ends because the reader cancelled it while its map was being
    /// published.
    #[test]
    fn a_confirmed_quit_that_waited_for_a_rebuild_exits_when_its_cancellation_lands() {
        let root = tempfile::tempdir().expect("scan rebuild root should exist");
        let mut owner = rebuilding_owner_without_workers(root.path());
        owner
            .cancel_pending_work_and_exit(true)
            .expect("the quit should be accepted");
        owner
            .handle_worker_event(WorkerEvent::ScanFinished { cancelled: false })
            .expect("the rebuild's scan should end");
        owner.app.suppress_generation_rebuild_restart();
        assert!(owner.app.is_running);

        owner
            .process_scan_store_events()
            .expect("the cancelled rebuild should settle");

        assert!(!owner.generation_rebuild_active);
        assert!(!owner.app.is_running);
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

    /// Proves the headless side of the contract between the scanner's worker-thread
    /// classification and the owner's string-only recognition: whatever
    /// `scan_store_capacity_message` produces for an out-of-room error, raw OS wording and
    /// all, `is_scan_store_capacity_failure` always recognizes, exactly as it does the
    /// session's own tracked quota (a strict no-op here) and never for an unrelated failure.
    #[test]
    fn capacity_message_and_failure_check_agree_on_a_raw_enospc() {
        let out_of_space = crate::scan_store::session::ScanStoreError::Run(
            crate::scan_store::run_file::RunError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "No space left on device (os error 28)",
            )),
        );
        let message = crate::scan_store::session::scan_store_capacity_message(&out_of_space);
        assert!(is_scan_store_capacity_failure(&message));

        let unrelated = crate::scan_store::session::ScanStoreError::Run(
            crate::scan_store::run_file::RunError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Permission denied (os error 13)",
            )),
        );
        let message = crate::scan_store::session::scan_store_capacity_message(&unrelated);
        assert!(!is_scan_store_capacity_failure(&message));
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the regression constructs a complete owner loop before asserting the capacity remedy"
    )]
    #[test]
    fn worker_capacity_failure_gets_the_scratch_space_remedy_not_an_unreadable_count() {
        let root = tempfile::tempdir().expect("test root should be created");
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
        let mut owner = OwnerLoop {
            app,
            input: Box::new(PendingInput),
            workers: None,
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(true, true, Duration::ZERO),
            frame_sink: None,
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
        };

        let out_of_space = crate::scan_store::session::ScanStoreError::Run(
            crate::scan_store::run_file::RunError::Io(std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "No space left on device (os error 28)",
            )),
        );
        let capacity_message =
            crate::scan_store::session::scan_store_capacity_message(&out_of_space);

        owner
            .handle_worker_event(WorkerEvent::ScanFailed {
                path: Some(root.path().join("entry-0")),
                message: capacity_message.clone(),
            })
            .expect("a capacity-classified scan failure should be handled");
        assert_eq!(owner.summary.unscanned_entries, 1);
        assert_eq!(
            owner.summary.unreadable_entries, 0,
            "a scan-store capacity failure is a store-wide problem, not one unreadable entry"
        );

        owner
            .handle_worker_event(WorkerEvent::ScanFailed {
                path: Some(root.path().join("entry-1")),
                message: capacity_message,
            })
            .expect("a repeated capacity failure should still be handled");
        assert_eq!(
            owner.summary.unscanned_entries, 1,
            "a second capacity failure must be a no-op, like headless's own \"first one wins\""
        );
        assert_eq!(owner.summary.unreadable_entries, 0);

        owner.app.finalize_scan();
        owner.app.start_ui();
        assert!(matches!(
            &owner.app.ui_mode,
            crate::UiMode::ScanResultsUnavailable(message)
                if message.contains("Not enough scratch space")
                    && message.contains("--scan-store-dir")
                    && message.contains("--scan-store-reserve-mib")
        ));
    }

    #[test]
    fn unrelated_worker_scan_failure_still_counts_as_unreadable() {
        let root = tempfile::tempdir().expect("test root should be created");
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
        let mut owner = OwnerLoop {
            app,
            input: Box::new(PendingInput),
            workers: None,
            clock: Box::new(VirtualClock::new()),
            animation: AnimationScheduler::new(true, true, Duration::ZERO),
            frame_sink: None,
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
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
        };

        owner
            .handle_worker_event(WorkerEvent::ScanFailed {
                path: Some(root.path().join("locked")),
                message: "Permission denied (os error 13)".to_string(),
            })
            .expect("an unrelated scan failure should be handled");
        assert_eq!(owner.summary.unreadable_entries, 1);
        assert_eq!(owner.summary.unscanned_entries, 1);
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
        assert!(deletion_progress_changed(&mut previous, 8, 0, false));
        assert!(!deletion_progress_changed(&mut previous, 8, 0, false));
        assert!(
            deletion_progress_changed(&mut previous, 8, 0, true),
            "starting mutation must refresh verification presentation even before progress advances"
        );
        assert!(!deletion_progress_changed(&mut previous, 8, 0, true));
        assert!(deletion_progress_changed(&mut previous, 8, 1, true));
        assert!(deletion_progress_changed(&mut previous, 16, 1, true));
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
    fn a_deletion_plan_refused_for_a_reason_that_is_known_names_it() {
        let io = |kind| DeletionPlanError::Io {
            path: "target".to_string(),
            message: "refused".to_string(),
            kind,
        };
        let general = "Deletion did not start: the selected item could not be checked";
        assert_eq!(
            deletion_plan_failure_notice(&io(std::io::ErrorKind::Other)),
            general
        );
        // Invalid data is also what a corrupt temporary file answers with, which is not the
        // file system's doing: it keeps the general notice.
        assert_eq!(
            deletion_plan_failure_notice(&io(std::io::ErrorKind::InvalidData)),
            general
        );

        for (error, reason) in [
            (io(std::io::ErrorKind::InvalidFilename), "too long"),
            (
                DeletionPlanError::Unrepresentable {
                    path: "target".to_string(),
                    message: "the file system reported a size of -1".to_string(),
                },
                "impossible value",
            ),
            (DeletionPlanError::Root, "mount point"),
        ] {
            let notice = deletion_plan_failure_notice(&error);
            assert!(
                notice.starts_with("Deletion did not start: ") && notice.contains(reason),
                "{error:?} should say why, not just that it could not be checked: {notice}"
            );
        }
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

    struct FlushFailsWriter;

    impl Write for FlushFailsWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[test]
    fn export_report_surfaces_a_flush_failure_instead_of_truncating_silently() {
        let error = export_report(FlushFailsWriter, |writer| {
            writer.write_all(b"{}").map_err(ReportError::from)
        })
        .expect_err("a flush failure must surface as an export failure, not vanish");
        assert!(error.contains("disk full"));
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

    /// F3: once one full sheen cycle has elapsed since the last input or state
    /// change, the selected tile's sheen stops requesting frames; input rearms
    /// it. This fails on the pre-fix behaviour, where `animate_selected_map`
    /// requests frames forever as long as a selection exists.
    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression builds a fully populated board before exercising the idle-arm window"
    )]
    fn the_sheen_settles_one_cycle_after_the_last_state_change_and_input_rearms_it() {
        let root = tempfile::tempdir().expect("test root should be created");
        let entry = root.path().join("entry");
        std::fs::write(&entry, b"x").expect("test entry should be created");
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
        // Populate the board directly (bypassing the scanner threads) with one
        // real, selectable entry, exactly as other owner-loop tests do.
        app.append_scan_store_entry_for_test(&entry_metadata, &entry, &entry_identity);
        app.finalize_scan();
        app.start_ui();
        let scan_view_root = app.current_folder_path();
        let mut owner = OwnerLoop {
            app,
            input: Box::new(IdleInput),
            workers: None,
            clock: Box::new(VirtualClock::new()),
            // Default motion (neither reduced nor monochrome): the selected
            // tile's sheen can animate, which this test is about.
            animation: AnimationScheduler::new(false, false, Duration::ZERO),
            frame_sink: None,
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
            scan_active: false,
            scheduler_snapshot: None,
            primary_scan_active: false,
            primary_reduction: None,
            scheduler_refreshed_at: None,
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
            stop_signals: None,
            signal_quit: false,
            forced_stop: false,
        };

        owner.app.mark_dirty();
        owner.render().expect("the post-scan render should succeed");
        let armed_at = owner.clock.now();
        assert!(
            owner.animation.next_frame_at().is_some(),
            "the selected tile's sheen should animate once a selection exists under default motion"
        );

        // One full cycle after the last state change (scan completion here)
        // with no further input, the sheen must settle: no more frames
        // requested.
        assert!(
            owner
                .clock
                .advance_to(armed_at.saturating_add(crate::animation::ONE_SHEEN_CYCLE))
        );
        assert!(
            owner
                .render_due_frame()
                .expect("the settling frame should render"),
            "the due animation frame should still render once, in its settled form"
        );
        assert!(
            owner.animation.next_frame_at().is_none(),
            "no more frames should be requested one cycle after the last state change"
        );
        assert!(!owner.animation.is_running());

        // Idle well past the settle point produces no further frames either.
        assert!(owner.clock.advance_to(
            armed_at.saturating_add(crate::animation::ONE_SHEEN_CYCLE.saturating_mul(2))
        ));
        assert!(
            !owner
                .render_due_frame()
                .expect("an idle frame should not fail"),
            "a settled sheen must not render again on its own"
        );
        assert!(owner.animation.next_frame_at().is_none());

        // Input re-arms the sheen for one more cycle.
        owner.input = Box::new(OnceKeyInput { delivered: false });
        owner
            .process_one_input()
            .expect("the keypress should be processed");
        owner.app.mark_dirty();
        owner.render().expect("the re-armed render should succeed");
        assert!(
            owner.animation.next_frame_at().is_some(),
            "input must re-arm the sheen for another cycle"
        );
    }
}
