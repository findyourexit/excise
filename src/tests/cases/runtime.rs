use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::error::AppError;
use crate::input::{InputEvent, InputSource};
use crate::outcome::OperationOutcome;
use crate::runtime::{Clock, RuntimeSettings, VirtualClock, run, scan_headless};
use crate::tests::cases::test_utils::test_backend_factory;
use crate::tests::fakes::{BackendOperation, TerminalEvent, TerminalEvents};

fn settings(root: &std::path::Path) -> RuntimeSettings {
    let metadata = std::fs::symlink_metadata(root).expect("runtime root metadata should exist");
    let root_identity = crate::native_path::identity_for(root, &metadata)
        .expect("runtime root identity should be readable")
        .expect("runtime root should not be a symbolic link");
    RuntimeSettings {
        root: root.to_path_buf(),
        root_identity,
        scan_threads: 1,
        event_capacity: 16,
        cross_filesystems: false,
        exclusions: Vec::new(),
        memory_mib: crate::model::DEFAULT_PROCESS_MIB,
        temporary_storage_mib: crate::temporary_storage::DEFAULT_TEMPORARY_STORAGE_MIB,
        apparent_size: true,
        disable_delete_confirmation: false,
        reduced_motion: true,
        monochrome: true,
        animate_loading: false,
        theme: crate::theme::ThemeId::ExciseDark,
        ascii: false,
        mouse: false,
        keymap: crate::config::KeyPreset::Vim,
        custom_keys: None,
        config_path: None,
        monochrome_locked: true,
    }
}

fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
    Event::Key(KeyEvent::new(code, modifiers))
}

#[test]
fn render_failure_returns_error_and_cleans_terminal() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let (events, _, backend) = test_backend_factory(80, 24);
    backend
        .failure_handle()
        .lock()
        .expect("failure queue should lock")
        .push_back(BackendOperation::Draw);

    let error = run(
        backend,
        Box::new(TerminalEvents::new(Vec::new())),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect_err("draw failure should escape the owner loop");
    assert!(matches!(
        error,
        AppError::Terminal {
            operation: "draw",
            ..
        }
    ));
    let events = events.lock().expect("terminal events should lock");
    assert_eq!(
        &events[events.len() - 2..],
        &[TerminalEvent::Clear, TerminalEvent::ShowCursor]
    );
}

struct FailingInput;

impl InputSource for FailingInput {
    fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
        Ok(true)
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        Err(AppError::io(
            "injected input failure",
            std::io::Error::other("input failed"),
        ))
    }
}

#[derive(Clone)]
struct SharedClock(Rc<Cell<Duration>>);

impl Clock for SharedClock {
    fn now(&self) -> Duration {
        self.0.get()
    }

    fn advance_to(&self, deadline: Duration) -> bool {
        if deadline > self.0.get() {
            self.0.set(deadline);
        }
        true
    }
}

struct TimeoutAfterEscInput {
    events: Vec<Option<Event>>,
    clock: Rc<Cell<Duration>>,
    draw_events: Arc<Mutex<Vec<String>>>,
    frames_at_escape: Arc<Mutex<Option<usize>>>,
    frames_before_quit: Arc<Mutex<Option<usize>>>,
    escaped: bool,
    waited_after_escape: bool,
}

impl TimeoutAfterEscInput {
    fn new(
        mut events: Vec<Option<Event>>,
        clock: Rc<Cell<Duration>>,
        draw_events: Arc<Mutex<Vec<String>>>,
        frames_at_escape: Arc<Mutex<Option<usize>>>,
        frames_before_quit: Arc<Mutex<Option<usize>>>,
    ) -> Self {
        events.reverse();
        Self {
            events,
            clock,
            draw_events,
            frames_at_escape,
            frames_before_quit,
            escaped: false,
            waited_after_escape: false,
        }
    }
}

impl InputSource for TimeoutAfterEscInput {
    fn poll(&mut self, timeout: Duration) -> Result<bool, AppError> {
        if self.escaped && !self.waited_after_escape {
            if timeout.is_zero() {
                return Ok(false);
            }
            self.waited_after_escape = true;
            self.clock.set(self.clock.get().saturating_add(timeout));
            return Ok(false);
        }
        Ok(!self.events.is_empty())
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        let event = self
            .events
            .pop()
            .ok_or_else(|| AppError::Invariant("fake input exhausted after poll".to_string()))?;
        if matches!(
            &event,
            Some(Event::Key(KeyEvent {
                code: KeyCode::Esc,
                ..
            }))
        ) {
            self.escaped = true;
            *self
                .frames_at_escape
                .lock()
                .expect("escape frame record should lock") = Some(
                self.draw_events
                    .lock()
                    .expect("draw event record should lock")
                    .len(),
            );
        }
        if matches!(
            &event,
            Some(Event::Key(KeyEvent {
                code: KeyCode::Char('q'),
                modifiers: KeyModifiers::NONE,
                ..
            }))
        ) {
            *self
                .frames_before_quit
                .lock()
                .expect("quit frame record should lock") = Some(
                self.draw_events
                    .lock()
                    .expect("draw event record should lock")
                    .len(),
            );
        }
        Ok(event.map_or(InputEvent::Barrier, InputEvent::Terminal))
    }
}

#[test]
fn input_failure_returns_error_and_cleans_terminal() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let (events, _, backend) = test_backend_factory(80, 24);
    let error = run(
        backend,
        Box::new(FailingInput),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect_err("input failure should escape the owner loop");
    assert!(matches!(error, AppError::Io { .. }));
    let events = events.lock().expect("terminal events should lock");
    assert_eq!(
        &events[events.len() - 2..],
        &[TerminalEvent::Clear, TerminalEvent::ShowCursor]
    );
}

#[test]
fn second_control_c_never_detaches_worker_state() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = TerminalEvents::new(vec![
        None,
        Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
        None,
        Some(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
    ]);
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("safe exit should restore cleanly");
    assert!(matches!(outcome, OperationOutcome::Exact(_)));
}

#[test]
fn graceful_quit_during_scan_is_precise_cancellation() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    for index in 0..200 {
        std::fs::write(root.path().join(format!("file-{index}")), b"x")
            .expect("fixture file should be written");
    }
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = TerminalEvents::new(vec![
        Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
    ]);
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("graceful cancellation should restore cleanly");
    assert!(
        matches!(outcome, OperationOutcome::Cancelled { precise: true, .. }),
        "unexpected outcome: {outcome:?}"
    );
}

#[test]
fn idle_navigation_animation_renders_before_following_keypress() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let folder = root.path().join("folder");
    let other_folder = root.path().join("other-folder");
    std::fs::create_dir(&folder).expect("fixture folder should exist");
    std::fs::create_dir(&other_folder).expect("second fixture folder should exist");
    std::fs::write(folder.join("child"), vec![0_u8; 8192]).expect("fixture child should exist");
    std::fs::write(other_folder.join("child"), vec![0_u8; 4096])
        .expect("second fixture child should exist");

    let (_, draw_events, backend) = test_backend_factory(120, 40);
    let clock = Rc::new(Cell::new(Duration::ZERO));
    let frames_at_escape = Arc::new(Mutex::new(None));
    let frames_before_quit = Arc::new(Mutex::new(None));
    let input = TimeoutAfterEscInput::new(
        vec![
            None,
            Some(key(KeyCode::Enter, KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
        ],
        clock.clone(),
        draw_events,
        frames_at_escape.clone(),
        frames_before_quit.clone(),
    );
    let mut runtime_settings = settings(root.path());
    runtime_settings.reduced_motion = false;

    let outcome = run(
        backend,
        Box::new(input),
        runtime_settings,
        Box::new(SharedClock(clock)),
    )
    .expect("navigation and exit should complete");
    assert!(matches!(outcome, OperationOutcome::Exact(_)));
    let frames_at_escape = frames_at_escape
        .lock()
        .expect("escape frame record should lock")
        .expect("escape should have been read");
    let frames_before_quit = frames_before_quit
        .lock()
        .expect("quit frame record should lock")
        .expect("quit should have been read");
    assert!(
        frames_before_quit >= frames_at_escape + 2,
        "the idle animation deadline must produce a frame before the next keypress: {frames_at_escape} -> {frames_before_quit}"
    );
}

#[allow(clippy::struct_excessive_bools)]
#[cfg(any(unix, windows))]
struct ReplanInput {
    events: Vec<Option<Event>>,
    target: std::path::PathBuf,
    changed: bool,
    remove_on_confirm: bool,
    change_on_plan: bool,
    remove_on_plan: bool,
    replace_directory_on_trigger: bool,
}

#[cfg(any(unix, windows))]
impl ReplanInput {
    fn new(events: Vec<Option<Event>>, target: std::path::PathBuf) -> Self {
        let mut events = events;
        events.reverse();
        Self {
            events,
            target,
            changed: false,
            remove_on_confirm: false,
            change_on_plan: false,
            remove_on_plan: false,
            replace_directory_on_trigger: false,
        }
    }
    fn missing(events: Vec<Option<Event>>, target: std::path::PathBuf) -> Self {
        let mut input = Self::new(events, target);
        input.remove_on_confirm = true;
        input
    }
    fn planning_change(events: Vec<Option<Event>>, target: std::path::PathBuf) -> Self {
        let mut input = Self::new(events, target);
        input.change_on_plan = true;
        input
    }

    fn planning_missing(events: Vec<Option<Event>>, target: std::path::PathBuf) -> Self {
        let mut input = Self::new(events, target);
        input.change_on_plan = true;
        input.remove_on_plan = true;
        input
    }

    #[cfg(unix)]
    fn planning_directory_replacement(
        events: Vec<Option<Event>>,
        target: std::path::PathBuf,
    ) -> Self {
        let mut input = Self::new(events, target);
        input.change_on_plan = true;
        input.replace_directory_on_trigger = true;
        input
    }
}

#[cfg(any(unix, windows))]
impl InputSource for ReplanInput {
    fn poll(&mut self, _timeout: Duration) -> Result<bool, AppError> {
        Ok(!self.events.is_empty())
    }

    fn read(&mut self) -> Result<InputEvent, AppError> {
        let event = self
            .events
            .pop()
            .ok_or_else(|| AppError::Invariant("fake input exhausted after poll".to_string()))?;
        let trigger = if self.change_on_plan {
            KeyCode::Backspace
        } else {
            KeyCode::Char('y')
        };
        if !self.changed
            && matches!(
                &event,
                Some(Event::Key(KeyEvent {
                    code,
                    modifiers: KeyModifiers::NONE,
                    ..
                })) if *code == trigger
            )
        {
            if self.replace_directory_on_trigger {
                let displaced = self.target.with_file_name("displaced-target");
                std::fs::rename(&self.target, displaced)
                    .expect("original directory should be displaced");
                std::fs::create_dir(&self.target).expect("replacement directory should be created");
                std::fs::write(self.target.join("replacement"), b"replacement")
                    .expect("replacement child should be written");
            } else if self.remove_on_confirm || self.remove_on_plan {
                std::fs::remove_file(&self.target).expect("post-plan target should be removed");
            } else {
                std::fs::write(&self.target, b"changed-after-plan")
                    .expect("post-plan replacement should be written");
            }
            self.changed = true;
        }
        Ok(event.map_or(InputEvent::Barrier, InputEvent::Terminal))
    }
}

#[cfg(unix)]
#[test]
fn changed_target_is_reported_without_reprompting() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let target = root.path().join("target");
    std::fs::write(&target, b"payload").expect("deletion target should be written");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = ReplanInput::new(
        vec![
            None,
            Some(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(key(KeyCode::Backspace, KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
        ],
        target.clone(),
    );
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("changed target should remain safely untouched");
    let OperationOutcome::Partial {
        completed_entries,
        failed_entries,
        value: summary,
    } = outcome
    else {
        panic!("changed target must not return exact: {outcome:?}");
    };
    assert_eq!(completed_entries, 0);
    assert_eq!(failed_entries, 1);
    assert_eq!(summary.deletion_changed_entries, 1);
    assert!(
        target.exists(),
        "changed target must not be retried from prior consent"
    );
}

#[cfg(unix)]
#[test]
fn replaced_directory_target_is_reported_without_reprompting() {
    let root = tempfile::tempdir().expect("deletion root should exist");
    let target = root.path().join("target");
    let displaced = root.path().join("displaced-target");
    std::fs::create_dir(&target).expect("deletion target should be created");
    std::fs::write(target.join("old-child"), b"old")
        .expect("initial directory child should be written");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = ReplanInput::planning_directory_replacement(
        vec![
            None,
            Some(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(key(KeyCode::Backspace, KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
        ],
        target.clone(),
    );
    let mut settings = settings(root.path());
    settings.disable_delete_confirmation = true;
    let outcome = run(
        backend,
        Box::new(input),
        settings,
        Box::new(VirtualClock::new()),
    )
    .expect("replaced directory should remain safely untouched");
    let OperationOutcome::Partial {
        completed_entries,
        failed_entries,
        value: summary,
    } = outcome
    else {
        panic!("replaced directory must not return exact: {outcome:?}");
    };
    assert_eq!(completed_entries, 0);
    assert_eq!(failed_entries, 1);
    assert_eq!(summary.deletion_changed_entries, 1);
    assert!(
        target.join("replacement").exists(),
        "replacement must not be deleted"
    );
    assert!(
        displaced.join("old-child").exists(),
        "old occupant should remain untouched"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn target_changed_before_confirmation_is_not_retried() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let target = root.path().join("target");
    std::fs::write(&target, b"payload").expect("deletion target should be written");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = ReplanInput::planning_change(
        vec![
            None,
            Some(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(key(KeyCode::Backspace, KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
        ],
        target.clone(),
    );
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("planning drift should not trigger a new deletion without consent");
    let OperationOutcome::Partial {
        completed_entries,
        failed_entries,
        value: summary,
    } = outcome
    else {
        panic!("changed target must not return exact: {outcome:?}");
    };
    assert_eq!(completed_entries, 0);
    assert_eq!(failed_entries, 1);
    assert_eq!(summary.deletion_changed_entries, 1);
    assert!(target.exists(), "changed target must remain untouched");
}

#[cfg(any(unix, windows))]
#[test]
fn missing_plan_target_reports_partial_summary() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let target = root.path().join("target");
    std::fs::write(&target, b"payload").expect("deletion target should be written");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = ReplanInput::planning_missing(
        vec![
            None,
            Some(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(key(KeyCode::Backspace, KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
        ],
        target.clone(),
    );
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("missing planning target should produce a partial summary");
    let OperationOutcome::Partial {
        completed_entries,
        failed_entries,
        value: summary,
    } = outcome
    else {
        panic!("missing planning target must not return exact: {outcome:?}");
    };
    assert_eq!(completed_entries, 0);
    assert_eq!(failed_entries, 1);
    assert_eq!(summary.deletion_missing_entries, 1);
    assert!(!target.exists(), "missing target should remain absent");
}

#[cfg(any(unix, windows))]
#[test]
fn missing_confirmed_target_reports_partial_summary() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let target = root.path().join("target");
    std::fs::write(&target, b"payload").expect("deletion target should be written");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = ReplanInput::missing(
        vec![
            None,
            Some(key(KeyCode::Down, KeyModifiers::NONE)),
            Some(key(KeyCode::Backspace, KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            None,
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
        ],
        target.clone(),
    );
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("missing final validation should produce a partial summary");
    let OperationOutcome::Partial {
        completed_entries,
        failed_entries,
        value: summary,
    } = outcome
    else {
        panic!("missing final validation must not return exact: {outcome:?}");
    };
    assert_eq!(completed_entries, 0);
    assert_eq!(failed_entries, 1);
    assert_eq!(summary.deletion_missing_entries, 1);
    assert!(!target.exists(), "missing target should remain absent");
}

#[cfg(unix)]
#[test]
fn skipped_link_is_an_explicit_scoped_boundary() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().expect("runtime root should exist");
    let outside = tempfile::tempdir().expect("outside root should exist");
    symlink(outside.path(), root.path().join("link")).expect("link should be created");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = TerminalEvents::new(vec![
        None,
        Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
        None,
        Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
    ]);
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("scoped scan should exit cleanly");
    let OperationOutcome::Exact(summary) = outcome else {
        panic!("expected exact scoped outcome, got {outcome:?}");
    };
    assert_eq!(summary.unscanned_entries, 1);
    assert_eq!(summary.link_entries, 1);
    assert_eq!(summary.unreadable_entries, 0);
}

#[test]
fn concurrent_interactive_scan_publishes_a_complete_bounded_generation() {
    const BRANCHES: usize = 8;
    const FILES_PER_BRANCH: usize = 64;

    let root = tempfile::tempdir().expect("scan root should exist");
    for branch in 0..BRANCHES {
        let directory = root.path().join(format!("branch-{branch:02}"));
        std::fs::create_dir(&directory).expect("branch should exist");
        for file in 0..FILES_PER_BRANCH {
            std::fs::write(
                directory.join(format!("file-{file:03}")),
                [branch as u8, file as u8],
            )
            .expect("fixture file should exist");
        }
    }
    let (_, _, backend) = test_backend_factory(100, 32);
    let input = TerminalEvents::new(vec![
        None,
        Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
        Some(key(KeyCode::Char('y'), KeyModifiers::NONE)),
    ]);
    let mut scan_settings = settings(root.path());
    scan_settings.scan_threads = 4;
    scan_settings.event_capacity = 16;
    scan_settings.memory_mib = crate::model::MIN_PROCESS_MIB;
    scan_settings.temporary_storage_mib = 64;
    let outcome = run(
        backend,
        Box::new(input),
        scan_settings,
        Box::new(VirtualClock::new()),
    )
    .expect("concurrent scan should finish cleanly");
    let OperationOutcome::Exact(summary) = outcome else {
        panic!("complete concurrent scan must remain exact: {outcome:?}");
    };
    assert_eq!(
        summary.scanned_entries,
        u64::try_from(BRANCHES.saturating_add(BRANCHES.saturating_mul(FILES_PER_BRANCH)))
            .expect("fixture count should fit")
    );
    assert_eq!(summary.unreadable_entries, 0);
    assert_eq!(summary.unscanned_entries, 0);
}

#[test]
fn headless_scan_streams_a_round_trippable_bounded_report() {
    let root = tempfile::tempdir().expect("headless root should exist");
    std::fs::write(root.path().join("zeta"), b"a").expect("first fixture should be written");
    std::fs::write(root.path().join("alpha"), b"bc").expect("second fixture should be written");

    let outcome = scan_headless(settings(root.path())).expect("headless scan should succeed");
    let OperationOutcome::Exact(report) = outcome else {
        panic!("expected exact headless report");
    };
    assert_eq!(report.summary().scanned_entries, 2);
    let mut encoded = Vec::new();
    report
        .write_json(&mut encoded)
        .expect("streamed report should serialize");
    let decoded: crate::report::ScanReportDocument =
        serde_json::from_slice(&encoded).expect("streamed report should deserialize");
    assert_eq!(decoded.document_kind, "scan-report");
    assert_eq!(decoded.entries.len(), 3);
    let paths = decoded
        .entries
        .iter()
        .map(|entry| entry.display_path.as_str())
        .collect::<Vec<_>>();
    assert!(
        paths.windows(2).all(|pair| pair[0] <= pair[1]),
        "streamed report paths must be deterministic and lexical: {paths:?}"
    );
}

#[test]
fn model_uncertainty_never_returns_an_exact_headless_exit() {
    let root = tempfile::tempdir().expect("headless root should exist");
    std::fs::write(root.path().join("excluded"), b"x").expect("excluded fixture should be written");
    let mut settings = settings(root.path());
    settings.exclusions = vec!["excluded".to_string()];

    let outcome = scan_headless(settings).expect("headless scan should complete");
    let OperationOutcome::Uncertain {
        unreadable_entries,
        value: report,
    } = outcome
    else {
        panic!("uncertain model must not return an exact outcome");
    };
    assert_eq!(unreadable_entries, 0);
    assert_eq!(report.state(), crate::report::ScanReportState::Uncertain);
}
