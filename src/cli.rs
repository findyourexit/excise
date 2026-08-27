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
use crate::report::{ReportError, ScanReport};
use crate::runtime::{RuntimeSettings, SystemClock, run, scan_headless};
use crate::terminal::{TerminalSession, validate_terminal};
use crate::theme::ThemeId;

#[allow(clippy::too_many_lines)]
pub(crate) fn run_main() -> i32 {
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
    if output_format != OutputFormat::Tui {
        let outcome = match scan_headless(settings) {
            Ok(outcome) => outcome,
            Err(error) => return report_error(&error),
        };
        let Some(report) = outcome.value() else {
            eprintln!("Error: headless scan returned no report");
            return ExitClass::Runtime.code();
        };
        if let Err(error) = write_scan_report(report, output_format, output_path.as_deref()) {
            return report_error(&error);
        }
        return outcome.exit_class().code();
    }
    if let Err(error) = validate_terminal() {
        return report_error(&error);
    }

    let mut session = match TerminalSession::enter_with_mouse(settings.mouse) {
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
    let backend = CrosstermBackend::new(io::stdout());
    let run_result = run(
        backend,
        Box::new(TerminalEvents),
        settings,
        Box::new(SystemClock::new()),
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
        let mut file = File::create(path)
            .map_err(|error| AppError::io("could not create report output", error))?;
        write_scan_report_to(report, format, &mut file)
    } else {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        write_scan_report_to(report, format, &mut output)
    }
}

fn write_scan_report_to(
    report: &ScanReport,
    format: OutputFormat,
    writer: &mut impl Write,
) -> Result<(), AppError> {
    let result = match format {
        OutputFormat::Json => report.write_json(writer),
        OutputFormat::Table => report.write_table(writer),
        OutputFormat::Tui => {
            return Err(AppError::Invariant(
                "TUI format reached headless report writer".to_string(),
            ));
        }
    };
    result.map_err(|error| match error {
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
}
