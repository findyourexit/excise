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
    /// The fixture is never cached, because a path-based removal such as `cargo clean` could not
    /// remove it where this cache is, or because the path of the cache cannot be resolved: see
    /// [`Fixtures::is_cacheable`](crate::fixture::Fixtures::is_cacheable). Take a run copy of it
    /// instead.
    #[error(
        "the fixture `{id}` is never cached: it holds a directory that cannot be listed or \
         changed, or it has, or can have, a path too long for the `PATH_MAX` that a path-based \
         removal such as `cargo clean` has to stay within, counting the path of the cache \
         directory above it as it is written, as it resolves, and as the system works on it \
         while it expands each link in it, or the path of the cache directory cannot be \
         resolved (a link in it leads nowhere, or a name in it is not a folder); take a run copy \
         of it instead"
    )]
    NotCacheable {
        /// The id of the fixture.
        id: String,
    },
    /// The fixture does its job only for a user that is not root, and the process is root: see
    /// [`FixtureSpec::needs_unprivileged_user`](crate::fixture::FixtureSpec::needs_unprivileged_user).
    #[error(
        "the fixture `{id}` needs a user that is not root: it holds a directory whose mode forbids \
         changes, root ignores modes, and so the directory would refuse root nothing; run as \
         another user"
    )]
    NeedsUnprivilegedUser {
        /// The id of the fixture.
        id: String,
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
