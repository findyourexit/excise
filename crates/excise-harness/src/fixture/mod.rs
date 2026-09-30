//! The fixture generator: deterministic, marked, verifiable file system trees for scenarios to run
//! against.
//!
//! # The pieces
//!
//! | Module | Role |
//! |---|---|
//! | [`spec`] | The TOML specification of a fixture: a default seed and one or more class generators. |
//! | [`plan`] | Expands a spec into the sorted, hashed manifest of every entry it creates, without touching the file system. |
//! | [`caps`] | Probes what the file system can do; entries that need a missing capability are skipped and flagged. |
//! | [`generate`] | Creates a plan's tree on disk with bounded parallelism and directory-relative system calls. |
//! | [`marker`] | The ownership marker every fixture root carries, and the check every runner makes. |
//! | [`cache`] | Generates once, verifies, and reuses. |
//! | [`run`] | Disposable per-run copies, and the [`Fixtures`] facade a runner calls. |
//! | [`oracle`] | An independent `lstat` walk: what is actually in a tree. |
//! | [`mutate`] | Live changes for a scenario's `fs_mutate` step. |
//! | [`volume`] | Constrained scratch volumes and mount boundaries, behind an explicit opt-in. |
//! | [`tree`] | Removing trees, including the ones a fixture made hard to remove. |
//!
//! # For runners
//!
//! A runner takes an already materialized fixture root plus a scenario. To get one:
//!
//! ```no_run
//! use excise_harness::fixture::Fixtures;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let scratch = std::env::temp_dir().join("my-run");
//! std::fs::create_dir_all(&scratch)?;
//! let fixtures = Fixtures::bundled();
//! // A disposable copy: generated fresh, marked, and removed (permissions restored first) on drop.
//! let run = fixtures.run_copy("node-modules-2k", &scratch)?;
//! let root = run.root(); // what excise is pointed at
//! # let _ = root;
//! # Ok(())
//! # }
//! ```
//!
//! The scenario names the fixture by id (`fixture = "node-modules-2k"`), the id names the spec
//! file `fixtures/node-modules-2k.toml`, and everything else follows from the spec.
//!
//! # Determinism
//!
//! Names and sizes are a pure function of the spec and its seed, produced by the crate's own
//! PRNG. There are no timestamps and no hash-map iteration order in any output. The same spec and
//! seed give the same manifest hash on every machine, and the same tree on every machine whose
//! file system can create every entry; the entries a file system cannot create are skipped and
//! reported (see [`caps`]).

pub mod cache;
pub mod caps;
pub mod error;
pub mod generate;
pub mod integrity;
pub mod marker;
pub mod mutate;
pub mod names;
pub mod oracle;
pub mod path;
pub mod plan;
mod rng;
pub mod run;
pub mod spec;
mod sys;
pub mod tree;
pub mod volume;

#[cfg(test)]
mod tests;

pub use cache::{CACHE_DIR_NAME, FixtureCache, MaterializeOptions, Materialized};
pub use caps::{Capabilities, Capability, CapabilityStatus};
pub use error::FixtureError;
pub use generate::{
    GenerateError, GenerateOptions, GenerateReport, MAX_THREADS, generate, generate_part,
};
pub use integrity::{IntegrityFailure, Verified, Verify, verify_master};
pub use marker::{
    Marker, MarkerError, OwnershipError, Role, read_marker, verify_owned, write_marker,
};
pub use names::NameStyle;
pub use oracle::{Comparison, Discrepancy, Oracle, OracleEntry, OracleError, OracleOptions};
pub use path::{LinkTarget, PathError, RelPath};
pub use plan::{Manifest, ManifestEntry, Plan, VolumePlan};
pub use run::{Fixtures, RunCopy};
pub use spec::{FixtureSpec, Part, SpecError};
pub use tree::{TreeGuard, remove_tree};
pub use volume::{PRIVILEGED_ENV, PrivilegedOptIn, Volume, VolumeError, VolumeSpec};

use crate::string_enum::string_enum;

/// The version of the generator's output. Bump it whenever the same spec and seed would produce
/// a different tree or manifest, so a cached fixture from an older generator is never reused.
pub const GENERATOR_VERSION: u32 = 1;

/// The name of the ownership marker at the root of every fixture: a regular file.
pub const MARKER_FILE_NAME: &str = ".excise-harness-owned";

string_enum! {
    /// The kind of a file system entry, as `lstat` reports it.
    pub enum NodeKind {
        /// A directory.
        Directory => "directory",
        /// A regular file.
        File => "file",
        /// A symbolic link. Never followed.
        Symlink => "symlink",
        /// Anything else: a FIFO, socket, or device. The generator never creates one.
        Other => "other",
    }
}
