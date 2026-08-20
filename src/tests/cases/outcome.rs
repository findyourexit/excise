use crate::error::{AppError, ExitClass};
use crate::outcome::{OperationOutcome, RunSummary};

#[test]
fn operation_outcomes_use_stable_exit_classes() {
    let summary = RunSummary::default();
    assert_eq!(
        OperationOutcome::Exact(summary.clone()).exit_class(),
        ExitClass::Exact
    );
    assert_eq!(
        OperationOutcome::Uncertain {
            value: summary.clone(),
            unreadable_entries: 1,
        }
        .exit_class(),
        ExitClass::Uncertain
    );
    assert_eq!(
        OperationOutcome::Partial {
            value: summary.clone(),
            completed_entries: 1,
            failed_entries: 1,
        }
        .exit_class(),
        ExitClass::Partial
    );
    assert_eq!(
        OperationOutcome::Cancelled {
            value: Some(summary),
            precise: false,
        }
        .exit_class(),
        ExitClass::Interrupted
    );
}

#[test]
fn fatal_errors_use_stable_exit_classes() {
    assert_eq!(
        AppError::Cli("bad".to_string()).exit_class(),
        ExitClass::Usage
    );
    assert_eq!(
        AppError::Config("bad".to_string()).exit_class(),
        ExitClass::Config
    );
    assert_eq!(
        AppError::io("bad", std::io::Error::other("bad")).exit_class(),
        ExitClass::Io
    );
    assert_eq!(
        AppError::Worker("bad".to_string()).exit_class(),
        ExitClass::Runtime
    );
}
