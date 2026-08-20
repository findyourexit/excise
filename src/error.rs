use std::io;

use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ExitClass {
    Exact = 0,
    Uncertain = 2,
    Partial = 3,
    Usage = 64,
    Runtime = 70,
    Io = 74,
    Config = 78,
    Interrupted = 130,
}

impl ExitClass {
    #[must_use]
    pub const fn code(self) -> i32 {
        self as i32
    }
}

#[derive(Debug, Error)]
pub enum AppError {
    #[error("invalid command line: {0}")]
    Cli(String),
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("terminal unavailable: {0}")]
    Tty(String),
    #[error("{context}: {source}")]
    Io {
        context: String,
        #[source]
        source: io::Error,
    },
    #[error("terminal {operation} failed: {message}")]
    Terminal {
        operation: &'static str,
        message: String,
    },
    #[error("worker failure: {0}")]
    Worker(String),
    #[error("model failure: {0}")]
    Model(String),
    #[error("internal invariant failed: {0}")]
    Invariant(String),
}

impl AppError {
    #[must_use]
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    #[must_use]
    pub fn terminal(operation: &'static str, error: impl std::fmt::Display) -> Self {
        Self::Terminal {
            operation,
            message: error.to_string(),
        }
    }

    #[must_use]
    pub const fn exit_class(&self) -> ExitClass {
        match self {
            Self::Cli(_) => ExitClass::Usage,
            Self::Config(_) => ExitClass::Config,
            Self::Io { .. } => ExitClass::Io,
            Self::Tty(_)
            | Self::Terminal { .. }
            | Self::Worker(_)
            | Self::Model(_)
            | Self::Invariant(_) => ExitClass::Runtime,
        }
    }
}
