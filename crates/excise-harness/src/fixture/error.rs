//! The error of materializing a fixture.

use std::io;

use thiserror::Error;

use crate::fixture::{
    generate::GenerateError, marker::MarkerError, oracle::OracleError, spec::SpecError,
    volume::VolumeError,
};

/// Why a fixture could not be materialized.
#[derive(Debug, Error)]
pub enum FixtureError {
    /// The spec could not be loaded, or breaks a rule.
    #[error(transparent)]
    Spec(#[from] SpecError),
    /// Generation failed.
    #[error(transparent)]
    Generate(#[from] GenerateError),
    /// A marker could not be read.
    #[error(transparent)]
    Marker(#[from] MarkerError),
    /// The oracle walk failed.
    #[error(transparent)]
    Oracle(#[from] OracleError),
    /// A scratch volume could not be attached.
    #[error(transparent)]
    Volume(#[from] VolumeError),
    /// A run copy was asked to regenerate a part the plan does not have.
    #[error("the fixture has no part named `{part}`; its parts are: {}", known.join(", "))]
    UnknownPart {
        /// The requested name.
        part: String,
        /// The names of the parts the plan has.
        known: Vec<String>,
    },
    /// A file system operation of the cache or a run copy failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The underlying error.
        source: io::Error,
    },
}
