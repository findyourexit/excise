//! Deterministic counts of what a build of `excise` costs (`cargo xtask counts`).
//!
//! Timing is measured only by paired A/B runs (`cargo xtask bench-e2e`). What this module counts
//! is the other kind of cost: numbers that, for a given binary and fixture, do not depend on
//! timing or load, so that two commits can be compared exactly and a difference is always real.
//!
//! * [`measure`] counts a fixed set of fixtures and writes one `harness-counts` document.
//! * [`compare`] compares the document of a pull request with the record of its base commit.
//! * [`comment`] renders that comparison as the pull request comment.
//! * [`history`] finds a base commit's record on the `bench-data` branch, and appends a new one.
//! * [`artifact`] reads a document from outside this build, which is untrusted input.

pub mod artifact;
pub mod comment;
pub mod compare;
pub mod history;
mod interactive;
pub mod measure;
#[cfg(test)]
mod test_support;
mod text;

pub use measure::{Commit, CountsError, CountsOptions, CountsReport, Progress, run_counts};

use crate::scenario::Profile;

/// The fixtures the suite counts, in the order a document lists them.
pub const FIXTURES: [&str; 4] = [
    "wide-1k",
    "node-modules-2k",
    "identity-small",
    "tiny-files-50k",
];

/// The profile every case runs under: reduced motion and one scan thread, so that no count
/// depends on how many processors the machine has.
pub const PROFILE: Profile = Profile::Deterministic;

/// The names of the counts the suite takes.
pub mod metric {
    /// The entries the scan covered, as the program's own report counts them. A count of the
    /// fixture rather than of the build: it moves only when the fixture or the accounting does.
    pub const ENTRIES: &str = "entries";
    /// The bytes the scan store held when the scan ended, as the program's own report states
    /// them.
    pub const SCAN_STORE_BYTES: &str = "scan_store_bytes";
    /// The files a headless scan and an interactive session left behind in their scratch areas.
    pub const RESIDUE_FILES: &str = "residue_files";
}
