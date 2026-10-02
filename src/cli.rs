//! Binary command-line orchestration for the private implementation.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;

use clap::Parser;
use clap::error::ErrorKind;
use ratatui::backend::CrosstermBackend;

use crate::config::{Cli, OutputFormat, RuntimeConfig, default_config_path};
use crate::error::{AppError, ExitClass};
use crate::input::TerminalEvents;
#[cfg(debug_assertions)]
use crate::native_path::safe_display_os_str_text;
use crate::native_path::{ResolvedRoot, safe_display_text};
use crate::report::{ReportError, ScanReport, write_buffered};
use crate::runtime::{
    RuntimeSettings, SystemClock, run_with_frame_gate, scan_headless_with_stop_signals,
};
use crate::signals::{self, StopRequest};
use crate::terminal::{SplitColorWriter, TerminalSession, spawn_frame_writer, validate_terminal};
use crate::test_events;
use crate::theme::ThemeId;
use crossbeam_channel::Receiver;

pub(crate) fn run_main() -> i32 {
    crate::os::raise_soft_descriptor_limit();
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            return if error.print().is_ok() {
                ExitClass::Exact.code()
            } else {
                ExitClass::Io.code()
            };
        }
        Err(error) => {
            eprintln!("{}", safe_error_text(error));
            return ExitClass::Usage.code();
        }
    };

    let config = match RuntimeConfig::load(cli) {
        Ok(config) => config,
        Err(error) => return report_error(&error),
    };
    let root = match ResolvedRoot::resolve(config.root.clone()) {
        Ok(root) => root,
        Err(error) => return report_error(&error),
    };
    let output_format = config.format;
    let output_path = config.output.clone();
    let preference_path = config.config_path.clone().or_else(default_config_path);
    let monochrome_locked = config.monochrome && config.theme != ThemeId::Monochrome;
    let settings = RuntimeSettings {
        root: root.resolved.as_path().to_path_buf(),
        root_identity: root.identity.clone(),
        scan_threads: config.scan_threads,
        event_capacity: config.event_buffer,
        cross_filesystems: config.cross_filesystems,
        exclusions: config.exclusions,
        memory_mib: config.memory_mib,
        temporary_storage_mib: config.temporary_storage_mib,
        scan_store_mib: config.scan_store_mib,
        scan_store_reserve_mib: config.scan_store_reserve_mib,
        scan_store_dir: config.scan_store_dir,
        apparent_size: config.apparent_size,
        disable_delete_confirmation: config.disable_delete_confirmation,
        reduced_motion: config.reduced_motion,
        monochrome: config.monochrome,
        animate_loading: true,
        theme: config.theme,
        ascii: config.ascii,
        mouse: config.mouse,
        keymap: config.keymap,
        custom_keys: config.custom_keys,
        config_path: preference_path,
        monochrome_locked,
    };
    if let Err(error) = test_events::init_from_env() {
        return report_error(&error);
    }
    let stop_signals = match signals::install() {
        Ok(stop_signals) => stop_signals,
        Err(error) => {
            return report_error(&AppError::io(
                "could not install the quit-signal handler",
                error,
            ));
        }
    };
    if output_format != OutputFormat::Tui {
        let code = run_headless(
            settings,
            stop_signals.receiver(),
            output_format,
            output_path.as_deref(),
        );
        stop_signals.acknowledge_done();
        return code;
    }
    let code = run_tui(settings, stop_signals.receiver());
    stop_signals.acknowledge_done();
    test_events::exit(code);
    code
}

fn run_headless(
    settings: RuntimeSettings,
    stop_signals: Receiver<StopRequest>,
    output_format: OutputFormat,
    output_path: Option<&Path>,
) -> i32 {
    // Held for one more check below, after `scan_headless_with_stop_signals` (which keeps its
    // own copy) returns: a request that lands after its last check (returning through the call
    // stack, or anything here before `write_scan_report`) still ends this run as a cancelled,
    // 130 exit instead of writing whatever a normal outcome produced.
    let late_stop_signal = stop_signals.clone();
    let outcome = match scan_headless_with_stop_signals(settings, Some(stop_signals)) {
        Ok(outcome) => outcome,
        Err(error) => return report_error(&error),
    };
    let Some(report) = outcome.value() else {
        eprintln!("Error: headless scan returned no report");
        return ExitClass::Runtime.code();
    };
    if late_stop_signal.try_recv().is_ok() {
        let cancelled = ScanReport::cancelled(
            report.root().to_path_buf(),
            report.root_identity().cloned(),
            report.summary().clone(),
        );
        return if let Err(error) = write_scan_report(&cancelled, output_format, output_path) {
            report_error(&error)
        } else {
            ExitClass::Interrupted.code()
        };
    }
    if let Err(error) = write_scan_report(report, output_format, output_path) {
        return report_error(&error);
    }
    outcome.exit_class().code()
}

fn run_tui(settings: RuntimeSettings, stop_signals: Receiver<StopRequest>) -> i32 {
    if let Err(error) = validate_terminal() {
        return report_error(&error);
    }
    let (frame_writer, frame_sink) = match spawn_frame_writer(io::stdout(), io::stdout()) {
        Ok(pair) => pair,
        Err(error) => {
            return report_error(&AppError::io(
                "could not start the terminal writer thread",
                error,
            ));
        }
    };

    let mut session = match TerminalSession::enter_with_mouse(settings.mouse, frame_sink.clone()) {
        Ok(session) => session,
        Err(error) => return report_error(&error),
    };
    #[cfg(debug_assertions)]
    assert!(
        std::env::var_os("EXCISE_TEST_PANIC_AFTER_TERMINAL_ENTRY").is_none(),
        "injected panic after terminal entry"
    );
    #[cfg(debug_assertions)]
    if let Some(error) = injected_runtime_error() {
        let restore_result = session.restore();
        drop(session);
        if let Err(restore_error) = restore_result {
            eprintln!("Error: {}", safe_error_text(&error));
            return report_error(&restore_error);
        }
        return report_error(&error);
    }
    let backend = CrosstermBackend::new(SplitColorWriter::new(frame_writer));
    let run_result = run_with_frame_gate(
        backend,
        Box::new(TerminalEvents::default()),
        settings,
        Box::new(SystemClock::new()),
        Some(frame_sink),
        Some(stop_signals),
    );
    let restore_result = session.restore();
    drop(session);

    match (run_result, restore_result) {
        (Ok(outcome), Ok(())) => outcome.exit_class().code(),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => report_error(&error),
        (Err(run_error), Err(restore_error)) => {
            eprintln!("Error: {}", safe_error_text(run_error));
            eprintln!(
                "Error while restoring terminal: {}",
                safe_error_text(restore_error)
            );
            ExitClass::Runtime.code()
        }
    }
}

fn write_scan_report(
    report: &ScanReport,
    format: OutputFormat,
    output: Option<&Path>,
) -> Result<(), AppError> {
    if let Some(path) = output {
        let file = File::create(path)
            .map_err(|error| AppError::io("could not create report output", error))?;
        write_scan_report_to(report, format, file)
    } else {
        write_scan_report_to(report, format, io::stdout().lock())
    }
}

fn write_scan_report_to(
    report: &ScanReport,
    format: OutputFormat,
    writer: impl Write,
) -> Result<(), AppError> {
    write_buffered(writer, |writer| match format {
        OutputFormat::Json => report.write_json(writer),
        OutputFormat::Table => report.write_table(writer),
        OutputFormat::Tui => Err(ReportError::Invariant(
            "TUI format reached headless report writer".to_string(),
        )),
    })
    .map_err(|error| match error {
        ReportError::Io(error) => AppError::io("could not write report", error),
        ReportError::Serialization(error) => {
            AppError::Invariant(format!("could not serialize report: {error}"))
        }
        ReportError::Invariant(message) => AppError::Invariant(message),
    })
}

fn report_error(error: &AppError) -> i32 {
    eprintln!("Error: {}", safe_error_text(error));
    error.exit_class().code()
}

#[cfg(debug_assertions)]
fn injected_runtime_error() -> Option<AppError> {
    let kind = std::env::var_os("EXCISE_TEST_ERROR_AFTER_TERMINAL_ENTRY")?;
    Some(match kind.to_str() {
        Some("input") => AppError::io("injected input failure", io::Error::other("input failed")),
        Some("render") => AppError::terminal("draw", "injected render failure"),
        Some("worker") => AppError::Worker("injected worker failure".to_string()),
        _ => AppError::Invariant(format!(
            "unknown injected failure {}",
            safe_display_os_str_text(&kind)
        )),
    })
}

fn safe_error_text(error: impl std::fmt::Display) -> String {
    let error = error.to_string();
    safe_display_text(&error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outcome::RunSummary;
    use std::path::PathBuf;

    #[test]
    fn report_error_text_escapes_controls_and_preserves_marker() {
        let rendered = safe_error_text(AppError::Config("bad\n\u{202e}name\u{1b}[31m".to_string()));
        assert!(rendered.starts_with("[deceptive]"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\u{202e}"));
        assert!(rendered.contains("\\x1b"));
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains('\u{202e}'));
    }

    #[test]
    fn report_error_keeps_missing_hostile_root_path_safe_once() {
        let parent = tempfile::tempdir().expect("root-error parent should exist");
        let path = parent.path().join("missing-\u{202e}root");
        let error = ResolvedRoot::resolve(path).expect_err("missing root should fail to resolve");

        let raw = error.to_string();
        let rendered = safe_error_text(&error);
        assert!(raw.contains("[deceptive]"));
        assert!(raw.contains("missing-\\u{202e}root"));
        assert_eq!(rendered, raw);
        assert_eq!(
            rendered
                .matches(crate::native_path::DECEPTIVE_DISPLAY_MARKER)
                .count(),
            1,
        );
        assert_eq!(rendered.matches("\\u{202e}").count(), 1);
        assert!(!rendered.chars().any(char::is_control));
        assert!(!rendered.contains('\u{202e}'));
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
    fn write_scan_report_to_surfaces_a_flush_failure() {
        let report = ScanReport::cancelled(PathBuf::from("/scan"), None, RunSummary::default());
        let error = write_scan_report_to(&report, OutputFormat::Json, FlushFailsWriter)
            .expect_err("a flush failure must surface, not be silently discarded");
        assert!(matches!(error, AppError::Io { .. }));
        assert_eq!(error.exit_class(), ExitClass::Io);
    }
}
