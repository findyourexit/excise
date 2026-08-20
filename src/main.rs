use std::io;
use std::process;

use clap::Parser;
use clap::error::ErrorKind;
use ratatui::backend::CrosstermBackend;

use excise::config::{Cli, RuntimeConfig};
use excise::error::{AppError, ExitClass};
use excise::native_path::ResolvedRoot;
use excise::runtime::{RuntimeSettings, SystemClock, run};
use excise::{TerminalEvents, TerminalSession, validate_terminal};

fn main() {
    process::exit(run_main());
}

fn run_main() -> i32 {
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
            eprintln!("{error}");
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
    if let Err(error) = validate_terminal() {
        return report_error(&error);
    }

    let mut session = match TerminalSession::enter() {
        Ok(session) => session,
        Err(error) => return report_error(&error),
    };
    #[cfg(debug_assertions)]
    assert!(
        std::env::var_os("EXCISE_TEST_PANIC_AFTER_TERMINAL_ENTRY").is_none(),
        "injected panic after terminal entry"
    );
    let settings = RuntimeSettings {
        root: root.resolved.as_path().to_path_buf(),
        scan_threads: config.scan_threads,
        event_capacity: config.event_buffer,
        apparent_size: config.apparent_size,
        disable_delete_confirmation: config.disable_delete_confirmation,
        reduced_motion: config.reduced_motion,
        monochrome: config.monochrome,
        animate_loading: true,
    };
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
            eprintln!("Error: {run_error}");
            eprintln!("Error while restoring terminal: {restore_error}");
            ExitClass::Runtime.code()
        }
    }
}

fn report_error(error: &AppError) -> i32 {
    eprintln!("Error: {error}");
    error.exit_class().code()
}
