use crate::error::ExitClass;

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RunSummary {
    pub scanned_entries: u64,
    pub identified_entries: u64,
    pub unreadable_entries: u64,
    pub deleted_entries: u64,
    pub last_unreadable_path: Option<String>,
    pub last_worker_error: Option<String>,
}
