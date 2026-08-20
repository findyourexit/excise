use crate::error::ExitClass;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OperationOutcome<T> {
    Exact(T),
    Uncertain {
        value: T,
        unreadable_entries: u64,
    },
    Partial {
        value: T,
        completed_entries: u64,
        failed_entries: u64,
    },
    Cancelled {
        value: Option<T>,
        precise: bool,
    },
}

impl<T> OperationOutcome<T> {
    #[must_use]
    pub const fn exit_class(&self) -> ExitClass {
        match self {
            Self::Exact(_) => ExitClass::Exact,
            Self::Uncertain { .. } => ExitClass::Uncertain,
            Self::Partial { .. } => ExitClass::Partial,
            Self::Cancelled { .. } => ExitClass::Interrupted,
        }
    }

    #[must_use]
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Exact(value) | Self::Uncertain { value, .. } | Self::Partial { value, .. } => {
                Some(value)
            }
            Self::Cancelled { value, .. } => value.as_ref(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunSummary {
    pub scanned_entries: u64,
    pub identified_entries: u64,
    pub unreadable_entries: u64,
    pub unscanned_entries: u64,
    pub excluded_entries: u64,
    pub filesystem_boundaries: u64,
    pub link_entries: u64,
    pub deleted_entries: u64,
    pub deletion_changed_entries: u64,
    pub deletion_missing_entries: u64,
    pub deletion_failed_entries: u64,
    pub deletion_unattempted_entries: u64,
    pub model_bytes: usize,
    pub model_limit_bytes: usize,
    pub identity_spilled: bool,
    pub last_unreadable_path: Option<String>,
    pub last_unscanned_path: Option<String>,
    pub last_unscanned_reason: Option<String>,
    pub last_worker_error: Option<String>,
}
