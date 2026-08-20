use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

use crate::error::AppError;
use crate::input::{InputEvent, InputSource};
use crate::outcome::OperationOutcome;
use crate::runtime::{RuntimeSettings, VirtualClock, run, scan_headless};
use crate::tests::cases::test_utils::test_backend_factory;
use crate::tests::fakes::{BackendOperation, TerminalEvent, TerminalEvents};

fn settings(root: &std::path::Path) -> RuntimeSettings {
    RuntimeSettings {
        root: root.to_path_buf(),
        scan_threads: 1,
        event_capacity: 16,
        cross_filesystems: false,
        exclusions: Vec::new(),
        memory_mib: crate::model::DEFAULT_PROCESS_MIB,
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
fn second_control_c_is_an_imprecise_hard_cancel() {
    let root = tempfile::tempdir().expect("runtime root should exist");
    let (_, _, backend) = test_backend_factory(80, 24);
    let input = TerminalEvents::new(vec![
        None,
        Some(key(KeyCode::Char('q'), KeyModifiers::NONE)),
        None,
        Some(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    ]);
    let outcome = run(
        backend,
        Box::new(input),
        settings(root.path()),
        Box::new(VirtualClock::new()),
    )
    .expect("hard cancellation should restore cleanly");
    assert!(matches!(
        outcome,
        OperationOutcome::Cancelled { precise: false, .. }
    ));
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
